// Set up a testnet containing:
// 4-nodes System and Application subnets, single unassigned node, single boundary node and a p8s (with grafana) VM.
// All nodes use the following resources: 4 vCPUs, 24GiB of RAM and 50 GiB disk.
//
// You can setup this testnet by executing the following commands:
//
//   $ gitlab-ci/container/container-run.sh
//   $ ict testnet medium -- --test_tmpdir=./medium
//
// The --test_tmpdir=./medium will store the test output in the specified directory.
// This is useful to have access to in case you need to SSH into an IC node for example like:
//
//   $ ssh -i medium/_tmp/*/setup/ssh/authorized_priv_keys/admin admin@$ipv6
//
// Note that you can get the $ipv6 address of the IC node by looking for a log line like:
//
//   Apr 11 15:34:10.175 INFO[rs/tests/src/driver/farm.rs:94:0]
//     VM(h2tf2-odxlp-fx5uw-kvn43-bam4h-i4xmw-th7l2-xxwvv-dxxpz-bs3so-iqe)
//     Host: ln1-dll10.ln1.dfinity.network
//     IPv6: 2a0b:21c0:4003:2:5051:85ff:feec:6864
//     vCPUs: 4
//     Memory: 25165824 KiB
//
// To get access to P8s and Grafana look for the following log lines:
//
//   Apr 11 15:33:58.903 INFO[rs/tests/src/driver/prometheus_vm.rs:168:0]
//     Prometheus Web UI at http://prometheus.medium--1681227226065.testnet.farm.dfinity.systems
//   Apr 11 15:33:58.903 INFO[rs/tests/src/driver/prometheus_vm.rs:169:0]
//     Grafana at http://grafana.medium--1681227226065.testnet.farm.dfinity.systems
//   Apr 11 15:33:58.903 INFO[rs/tests/src/driver/prometheus_vm.rs:170:0]
//     IC Progress Clock at http://grafana.medium--1681227226065.testnet.farm.dfinity.systems/d/ic-progress-clock/ic-progress-clock?refresh=10s&from=now-5m&to=now
//
// Happy testing!

use anyhow::Result;

use ic_registry_subnet_type::SubnetType;
use ic_tests::driver::{
    boundary_node::BoundaryNode,
    group::SystemTestGroup,
    ic::{InternetComputer, Subnet},
    prometheus_vm::{HasPrometheus, PrometheusVm},
    test_env::TestEnv,
    test_env_api::{
        await_boundary_node_healthy, HasTopologySnapshot, NnsCanisterWasmStrategy,
        NnsCustomizations,
    },
};
use ic_tests::orchestrator::utils::rw_message::install_nns_with_customizations_and_check_progress;

const BOUNDARY_NODE_NAME: &str = "boundary-node-1";

fn main() -> Result<()> {
    SystemTestGroup::new()
        .with_setup(setup)
        .execute_from_args()?;
    Ok(())
}

pub fn setup(env: TestEnv) {
    PrometheusVm::default()
        .start(&env)
        .expect("Failed to start prometheus VM");
    InternetComputer::new()
        .add_subnet(Subnet::new(SubnetType::System).add_nodes(4))
        .add_subnet(Subnet::new(SubnetType::Application).add_nodes(4))
        .with_unassigned_nodes(1)
        .setup_and_start(&env)
        .expect("Failed to setup IC under test");
    install_nns_with_customizations_and_check_progress(
        env.topology_snapshot(),
        NnsCanisterWasmStrategy::TakeBuiltFromSources,
        NnsCustomizations::default(),
    );
    BoundaryNode::new(String::from(BOUNDARY_NODE_NAME))
        .allocate_vm(&env)
        .expect("Allocation of BoundaryNode failed.")
        .for_ic(&env, "")
        .use_real_certs_and_dns()
        .start(&env)
        .expect("failed to setup BoundaryNode VM");
    env.sync_prometheus_config_with_topology();
    await_boundary_node_healthy(&env, BOUNDARY_NODE_NAME);
}
