/* tag::catalog[]
Title:: XNet messaging functions within SLO.

Goal:: Ensure IC routes XNet traffic in a timely manner.

Runbook::
0. Instantiate an IC with N application subnets containing M nodes each.
1. Install Xnet canisters on each subnet (number of canisters is calculated dynamically).
2. Start all canisters (via update `start` call).
3. Wait for RUNTIME_SEC secs for canisters to exchange messages.
4. Stop sending messages for all canisters (via update `stop` call).
5. Collect metrics from all canisters (via query `metrics` call).
6. Aggregate metrics for each subnet (over its canisters).
7. Assert error_ratio < 5%, no seq_errors, send_rate >= 0.3, responses_received > threshold (calculated dynamically).
8. Stop/delete all canisters and assert operations' success.

Success::
1. Xnet canisters are successfully installed and started on each subnet.
2. Metrics collected for subnets are within the limits.

Notes::
If the NNS canisters are not deployed, the subnets will stop making progress after 50min, therefore the test either needs to be short enough in this case.

end::catalog[] */

use super::common::{install_canisters, parallel_async, start_all_canisters};
use crate::driver::ic::{InternetComputer, Subnet};
use crate::driver::pot_dsl::{PotSetupFn, SysTestFn};
use crate::driver::test_env::TestEnv;
use crate::driver::test_env_api::{
    HasPublicApiUrl, HasTopologySnapshot, IcNodeContainer, NnsInstallationBuilder,
};
use crate::util::{block_on, runtime_from_url};
use canister_test::{Canister, Runtime};
use dfn_candid::candid;
use futures::future::join_all;
use ic_registry_subnet_type::SubnetType;
use slog::info;
use std::fmt::Display;
use std::time::Duration;
use xnet_test::Metrics;

// Constants for all xnet tests.
const PAYLOAD_SIZE_BYTES: u64 = 1024;
/// Maximum messages a canister should send every round (in order to prevent it
/// filling up its output queue). This should be estimated as:
///
/// `queue_capacity / 10 /* max_rounds roundtrip */`
const MAX_CANISTER_TO_CANISTER_RATE: usize = 30;
const SEND_RATE_THRESHOLD: f64 = 0.3;
const ERROR_PERCENTAGE_THRESHOLD: f64 = 5.0;
const TARGETED_LATENCY_SECONDS: u64 = 20;

#[derive(Debug, Clone)]
pub struct Config {
    subnets: usize,
    nodes_per_subnet: usize,
    runtime: Duration,
    payload_size_bytes: u64,
    send_rate_threshold: f64,
    error_percentage_threshold: f64,
    targeted_latency_seconds: u64,
    subnet_to_subnet_rate: usize,
    canisters_per_subnet: usize,
    canister_to_subnet_rate: usize,
}

impl Config {
    pub fn new(subnets: usize, nodes_per_subnet: usize, runtime: Duration, rate: usize) -> Config {
        // Subnet-to-subnet request rate: ceil(rate / subnet_connections).
        let subnet_to_subnet_rate = (rate - 1) / (subnets - 1) + 1;
        // Minimum number of subnet-to-subnet queues needed to stay under
        // `max_canister_to_canister_rate`.
        let subnet_to_subnet_queues =
            (subnet_to_subnet_rate - 1) / MAX_CANISTER_TO_CANISTER_RATE + 1;
        // Minimum number of canisters required to send `subnet_to_subnet_rate` requests
        // per round.
        let canisters_per_subnet = (subnet_to_subnet_queues as f64).sqrt().ceil() as usize;
        // A canister's outbound request rate to a given subnet.
        let canister_to_subnet_rate = (subnet_to_subnet_rate - 1) / canisters_per_subnet + 1;

        Config {
            subnets,
            nodes_per_subnet,
            runtime,
            payload_size_bytes: PAYLOAD_SIZE_BYTES,
            send_rate_threshold: SEND_RATE_THRESHOLD,
            error_percentage_threshold: ERROR_PERCENTAGE_THRESHOLD,
            targeted_latency_seconds: TARGETED_LATENCY_SECONDS,
            subnet_to_subnet_rate,
            canisters_per_subnet,
            canister_to_subnet_rate,
        }
    }

    /// Builds the IC instance.
    pub fn build(self) -> impl PotSetupFn {
        move |env: TestEnv| setup(env, self)
    }

    /// Returns a test function based on this configuration.
    pub fn test(self) -> impl SysTestFn {
        move |env: TestEnv| test(env, self)
    }
}

// Generic setup
fn setup(env: TestEnv, config: Config) {
    (0..config.subnets)
        .fold(InternetComputer::new(), |ic, _idx| {
            ic.add_subnet(Subnet::new(SubnetType::Application).add_nodes(config.nodes_per_subnet))
        })
        .setup_and_start(&env)
        .expect("failed to setup IC under test");
    env.topology_snapshot().subnets().for_each(|subnet| {
        subnet
            .nodes()
            .for_each(|node| node.await_status_is_healthy().unwrap())
    });
}

pub fn test(env: TestEnv, config: Config) {
    block_on(test_async(env, config));
}

// Generic test
pub async fn test_async(env: TestEnv, config: Config) {
    let logger = env.logger();
    info!(logger, "Config for the test: {:?}", config);
    let topology = env.topology_snapshot();
    // Install NNS for long tests (note that for large numbers of subnets or
    // nodes the registry might be too big for installation as a canister)
    if config.runtime > Duration::from_secs(1200) {
        info!(logger, "Installing NNS canisters on the root subnet...");
        let nns_node = topology.root_subnet().nodes().next().unwrap();
        NnsInstallationBuilder::new()
            .install(&nns_node, &env)
            .expect("Could not install NNS canisters");
        info!(&logger, "NNS canisters installed successfully.");
    }
    // Installing canisters on a subnet requires an Agent (or a Runtime wrapper around Agent).
    // We need only one agent (runtime) per subnet for canister installation.
    let endpoints_runtime: Vec<Runtime> = topology
        .subnets()
        .map(|s| {
            let node = s.nodes().next().unwrap();
            runtime_from_url(node.get_public_url(), node.effective_canister_id())
        })
        .collect();
    assert_eq!(endpoints_runtime.len(), config.subnets);
    // Step 1: Install Xnet canisters on each subnet.
    info!(logger, "Installing Xnet canisters on subnets ...");
    let canisters = install_canisters(
        env,
        &endpoints_runtime,
        config.subnets,
        config.canisters_per_subnet,
    )
    .await;
    let canisters_count = canisters.iter().map(Vec::len).sum::<usize>();
    assert_eq!(
        canisters_count,
        config.subnets * config.canisters_per_subnet
    );
    info!(
        logger,
        "All {} canisters installed successfully.", canisters_count
    );
    // Step 2: Start all canisters (via update `start` call).
    info!(logger, "Calling start() on all canisters...");
    start_all_canisters(
        &canisters,
        config.payload_size_bytes,
        config.canister_to_subnet_rate as u64,
    )
    .await;
    let msgs_per_round =
        config.canister_to_subnet_rate * config.canisters_per_subnet * (config.subnets - 1);
    info!(
        logger,
        "Starting chatter: {} messages/round * {} bytes = {} bytes/round",
        msgs_per_round,
        config.payload_size_bytes,
        msgs_per_round * config.payload_size_bytes as usize
    );
    // Step 3: Wait for canisters to exchange messages.
    info!(
        logger,
        "Sending messages for {} secs...",
        config.runtime.as_secs()
    );

    tokio::time::sleep(Duration::from_secs(config.runtime.as_secs())).await;

    // Step 4: Stop all canisters (via update `stop` call).
    info!(logger, "Stopping all canisters...");
    stop_all_canister(&canisters).await;
    // Step 5: Collect metrics from all canisters (via query `metrics` call).
    info!(logger, "Collecting metrics from all canisters...");
    let metrics = collect_metrics(&canisters).await;
    // Step 6: Aggregate metrics for each subnet (over its canisters).
    info!(logger, "Aggregating metrics for each subnet...");
    let mut aggregated_metrics = Vec::<Metrics>::new();
    for (subnet_idx, subnet_metrics) in metrics.iter().enumerate() {
        let mut merged_metric = Metrics::default();
        for (canister_idx, canister_metric) in subnet_metrics.iter().enumerate() {
            info!(
                logger,
                "Metrics for subnet {}, canister {}: {:?}",
                subnet_idx,
                canister_idx,
                canister_metric
            );
            merged_metric.merge(canister_metric);
        }
        aggregated_metrics.push(merged_metric);
        info!(
            logger,
            "Aggregated metrics for subnet {}: {:?}",
            subnet_idx,
            aggregated_metrics.last()
        );
    }
    // Step 7. Assert metric are within limits.
    info!(logger, "Asserting metrics are within limits...");
    let mut success = true;
    let mut expect =
        |cond: bool, subnet: usize, ok_msg: &str, fail_msg: &str, val: &dyn Display| {
            success &= cond;
            info!(
                logger,
                "Subnet {}: {} {}: {}",
                subnet,
                if cond { "Success:" } else { "Failure:" },
                if cond { ok_msg } else { fail_msg },
                val,
            );
        };

    for (i, m) in aggregated_metrics.iter().enumerate() {
        let attempted_calls = m.requests_sent + m.call_errors;
        if attempted_calls != 0 {
            let failed_calls = m.call_errors + m.reject_responses;
            let error_percentage = 100. * failed_calls as f64 / attempted_calls as f64;
            expect(
                error_percentage < config.error_percentage_threshold,
                i,
                format!("Error ratio below {}%", config.error_percentage_threshold).as_str(),
                "Failed calls",
                &format!(
                    "{}% ({}/{})",
                    error_percentage, failed_calls, attempted_calls
                ),
            );
        }

        expect(
            m.seq_errors == 0,
            i,
            "Sequence errors",
            "Sequence errors",
            &m.seq_errors,
        );

        let send_rate = attempted_calls as f64
            / (config.subnets - 1) as f64
            / config.runtime.as_secs() as f64
            / config.canisters_per_subnet as f64
            / config.canister_to_subnet_rate as f64;
        expect(
            send_rate >= config.send_rate_threshold,
            i,
            format!("Send rate at least {}", config.send_rate_threshold).as_str(),
            format!("Send rate below {}", config.send_rate_threshold).as_str(),
            &send_rate,
        );

        // Successful plus reject responses.
        let responses_received =
            m.latency_distribution.buckets().last().unwrap().1 + m.reject_responses;
        // All messages sent more than `targeted_latency_seconds` before the end of the
        // test should have gotten a response.
        let responses_expected = (m.requests_sent as f64
            * (config.runtime.as_secs() - config.targeted_latency_seconds) as f64
            / config.runtime.as_secs() as f64) as usize;
        // Account for requests enqueued this round (in case canister messages were
        // executed before ingress messages, i.e. the heartbeat was executed before
        // metrics collection) or uncounted responses (if ingress executed first).
        info!(
            logger,
            "responses_expected={} subnet_to_subnet_rate={}, responses_received={}",
            responses_expected,
            config.subnet_to_subnet_rate,
            responses_received
        );
        let responses_expected = responses_expected - config.subnet_to_subnet_rate;
        let actual = format!("{}/{}", responses_received, m.requests_sent);
        let msg = format!(
            "Expected requests sent more than {}s ago ({}/{}) to receive responses",
            config.targeted_latency_seconds, responses_expected, m.requests_sent
        );
        expect(
            responses_received >= responses_expected,
            i,
            &msg,
            &msg,
            &actual,
        );

        if responses_received != 0 {
            let avg_latency_millis = m.latency_distribution.sum_millis() / responses_received;
            expect(
                avg_latency_millis <= config.targeted_latency_seconds as usize * 1000,
                i,
                &format!(
                    "Mean response latency less than {}s",
                    config.targeted_latency_seconds
                ),
                &format!(
                    "Mean response latency was more than {}s",
                    config.targeted_latency_seconds
                ),
                &(avg_latency_millis as f64 * 1e-3),
            );
        }
    }
    info!(logger, "Stop/delete all canisters...");
    // Step 8: Stop all canisters.
    let _: Vec<_> = parallel_async(
        canisters.iter().flatten(),
        |canister| {
            info!(logger, "Stopping canister {} ...", canister.canister_id());
            canister.stop()
        },
        |_, res| {
            res.expect("Stopping canister failed.");
        },
    )
    .await;

    // Step 9: Delete all canisters.
    let _: Vec<_> = parallel_async(
        canisters.iter().flatten(),
        |canister| {
            info!(logger, "Deleting canister {} ...", canister.canister_id());
            canister.delete()
        },
        |_, res| {
            res.expect("Deleting canister failed.");
        },
    )
    .await;

    assert!(success, "Test failed.");
}

pub async fn stop_all_canister(canisters: &[Vec<Canister<'_>>]) {
    let mut futures = vec![];
    for (subnet_idx, canister_idx, canister) in canisters
        .iter()
        .enumerate()
        .flat_map(|(x, v)| v.iter().enumerate().map(move |(y, v)| (x, y, v)))
    {
        futures.push(async move {
            let _: String = canister
                .update_("stop", candid, ())
                .await
                .unwrap_or_else(|_| {
                    panic!(
                        "Stopping canister_idx={} on subnet_idx={} failed.",
                        canister_idx, subnet_idx
                    )
                });
        });
    }
    futures::future::join_all(futures).await;
}

pub async fn collect_metrics(canisters: &[Vec<Canister<'_>>]) -> Vec<Vec<Metrics>> {
    let mut futures: Vec<Vec<_>> = Vec::new();
    for (subnet_idx, canister_idx, canister) in canisters
        .iter()
        .enumerate()
        .flat_map(|(x, v)| v.iter().enumerate().map(move |(y, v)| (x, y, v)))
    {
        if canister_idx == 0 {
            futures.push(vec![]);
        }
        futures[subnet_idx].push(async move {
            canister
                .query_("metrics", candid, ())
                .await
                .unwrap_or_else(|_| {
                    panic!(
                        "Collecting metrics for canister_idx={} on subnet_idx={} failed.",
                        canister_idx, subnet_idx
                    )
                })
        });
    }
    join_all(futures.into_iter().map(|x| async { join_all(x).await })).await
}
