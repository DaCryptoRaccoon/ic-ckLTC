use candid::CandidType;
use dfn_protobuf::ProtoBuf;
use ic_base_types::{CanisterId, PrincipalId};
use ic_crypto_sha::Sha256;
pub use ic_ledger_canister_core::archive::ArchiveOptions;
use ic_ledger_canister_core::ledger::{LedgerContext, LedgerTransaction, TxApplyError};
use ic_ledger_core::{
    approvals::Approvals,
    balances::Balances,
    block::{BlockType, EncodedBlock, FeeCollector},
    tokens::CheckedAdd,
};
use ic_ledger_hash_of::HashOf;
use ic_ledger_hash_of::HASH_LENGTH;
use icrc_ledger_types::icrc1::account::Account;
use on_wire::{FromWire, IntoWire};
use serde::{Deserialize, Serialize};
use serde_bytes::ByteBuf;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::convert::TryFrom;
use std::fmt;
use std::time::Duration;
use strum_macros::IntoStaticStr;

pub use ic_ledger_core::{
    block::BlockIndex,
    timestamp::TimeStamp,
    tokens::{Tokens, TOKEN_SUBDIVIDABLE_BY},
};

pub mod account_identifier;
#[allow(clippy::all)]
#[path = "gen/ic_ledger.pb.v1.rs"]
pub mod protobuf;
mod validate_endpoints;
pub use account_identifier::{AccountIdentifier, Subaccount};
pub use validate_endpoints::{tokens_from_proto, tokens_into_proto};

/// Note that the Ledger can be deployed with a
/// different transaction fee. Clients that want to use the Ledger should query
/// for the fee before doing transactions.
pub const DEFAULT_TRANSFER_FEE: Tokens = Tokens::from_e8s(10_000);

pub const MAX_BLOCKS_PER_REQUEST: usize = 2000;

pub const MEMO_SIZE_BYTES: usize = 32;

pub type LedgerBalances = Balances<HashMap<AccountIdentifier, Tokens>>;

#[derive(
    Serialize,
    Deserialize,
    CandidType,
    Clone,
    Copy,
    Default,
    Hash,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
)]
pub struct Memo(pub u64);

pub type Certification = Option<Vec<u8>>;

/// An operation which modifies account balances
#[derive(
    Serialize,
    Deserialize,
    CandidType,
    Clone,
    Hash,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    IntoStaticStr,
)]
pub enum Operation {
    Burn {
        from: AccountIdentifier,
        amount: Tokens,
    },
    Mint {
        to: AccountIdentifier,
        amount: Tokens,
    },
    Transfer {
        from: AccountIdentifier,
        to: AccountIdentifier,
        amount: Tokens,
        fee: Tokens,
    },
    Approve {
        from: AccountIdentifier,
        spender: AccountIdentifier,
        allowance: Tokens,
        expires_at: Option<TimeStamp>,
        fee: Tokens,
    },
    TransferFrom {
        from: AccountIdentifier,
        to: AccountIdentifier,
        spender: AccountIdentifier,
        amount: Tokens,
        fee: Tokens,
    },
}

pub fn apply_operation<C>(
    context: &mut C,
    operation: &Operation,
    now: TimeStamp,
) -> Result<(), TxApplyError<C::Tokens>>
where
    C: LedgerContext<AccountId = AccountIdentifier, Tokens = Tokens>,
{
    match operation {
        Operation::Transfer {
            from,
            to,
            amount,
            fee,
        } => context
            .balances_mut()
            .transfer(from, to, *amount, *fee, None)?,
        Operation::Burn { from, amount, .. } => context.balances_mut().burn(from, *amount)?,
        Operation::Mint { to, amount, .. } => context.balances_mut().mint(to, *amount)?,
        Operation::Approve {
            from,
            spender,
            allowance,
            expires_at,
            fee,
        } => {
            // NB. We cannot reliably detect self-approvals at this level
            // because the approver and the spender principals are hashed.
            // We rely on the approve endpoint to perform this check.

            context.balances_mut().burn(from, *fee)?;

            let result = context
                .approvals_mut()
                .approve(from, spender, *allowance, *expires_at, now, None)
                .map_err(TxApplyError::from);
            if let Err(e) = result {
                context
                    .balances_mut()
                    .mint(from, *fee)
                    .expect("bug: failed to refund approval fee");
                return Err(e);
            }
        }

        Operation::TransferFrom {
            from,
            to,
            spender,
            amount,
            fee,
        } => {
            if from == spender {
                // NB. We bypass the allowance check if the account owner calls
                // transfer_from.

                // NB. We cannot reliably detect self-transfers at this level.
                // We need help from the transfer_from endpoint to populate
                // [from] and [spender] with equal values if the spender is the
                // account owner.
                context
                    .balances_mut()
                    .transfer(from, to, *amount, *fee, None)?;
                return Ok(());
            }

            let allowance = context.approvals().allowance(from, spender, now);
            if allowance.amount < *amount {
                return Err(TxApplyError::InsufficientAllowance {
                    allowance: allowance.amount,
                });
            }
            context
                .balances_mut()
                .transfer(from, to, *amount, *fee, None)?;
            context
                .approvals_mut()
                .use_allowance(from, spender, *amount, now)
                .expect("bug: cannot use allowance");
        }
    };
    Ok(())
}

/// An operation with the metadata the client generated attached to it
#[derive(
    Serialize, Deserialize, CandidType, Clone, Hash, Debug, PartialEq, Eq, PartialOrd, Ord,
)]
pub struct Transaction {
    pub operation: Operation,
    pub memo: Memo,
    /// The time this transaction was created.
    pub created_at_time: Option<TimeStamp>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icrc1_memo: Option<ByteBuf>,
}

impl LedgerTransaction for Transaction {
    type AccountId = AccountIdentifier;
    type Tokens = Tokens;

    fn burn(
        from: Self::AccountId,
        _spender: Option<Self::AccountId>,
        amount: Tokens,
        created_at_time: Option<TimeStamp>,
        memo: Option<u64>,
    ) -> Self {
        Self {
            operation: Operation::Burn { from, amount },
            memo: memo.map(Memo).unwrap_or_default(),
            icrc1_memo: None,
            created_at_time,
        }
    }

    fn created_at_time(&self) -> Option<TimeStamp> {
        self.created_at_time
    }

    fn hash(&self) -> HashOf<Self> {
        let mut state = Sha256::new();
        state.write(&serde_cbor::ser::to_vec_packed(&self).unwrap());
        HashOf::new(state.finish())
    }

    fn apply<C>(
        &self,
        context: &mut C,
        now: TimeStamp,
        _effective_fee: C::Tokens,
    ) -> Result<(), TxApplyError<C::Tokens>>
    where
        C: LedgerContext<AccountId = Self::AccountId, Tokens = Tokens>,
    {
        apply_operation(context, &self.operation, now)
    }
}

impl Transaction {
    pub fn new(
        from: AccountIdentifier,
        to: AccountIdentifier,
        amount: Tokens,
        fee: Tokens,
        memo: Memo,
        created_at_time: TimeStamp,
    ) -> Self {
        let operation = Operation::Transfer {
            from,
            to,
            amount,
            fee,
        };
        Transaction {
            operation,
            memo,
            icrc1_memo: None,
            created_at_time: Some(created_at_time),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ApprovalKey(AccountIdentifier, AccountIdentifier);

impl From<(&AccountIdentifier, &AccountIdentifier)> for ApprovalKey {
    fn from((account, spender): (&AccountIdentifier, &AccountIdentifier)) -> Self {
        Self(*account, *spender)
    }
}

/// A transaction with the metadata the canister generated attached to it
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub parent_hash: Option<HashOf<EncodedBlock>>,
    pub transaction: Transaction,
    /// Nanoseconds since the Unix epoch.
    pub timestamp: TimeStamp,
}

impl Block {
    pub fn new(
        parent_hash: Option<HashOf<EncodedBlock>>,
        operation: Operation,
        memo: Memo,
        created_at_time: TimeStamp, // transaction timestamp
        timestamp: TimeStamp,       // block timestamp
        effective_fee: Tokens,
    ) -> Result<Self, String> {
        let transaction = Transaction {
            operation,
            memo,
            icrc1_memo: None,
            created_at_time: Some(created_at_time),
        };
        Ok(Self::from_transaction(
            parent_hash,
            transaction,
            timestamp,
            effective_fee,
            None,
        ))
    }

    #[inline]
    pub fn new_from_transaction(
        parent_hash: Option<HashOf<EncodedBlock>>,
        transaction: Transaction,
        timestamp: TimeStamp,
        effective_fee: Tokens,
    ) -> Self {
        Self::from_transaction(parent_hash, transaction, timestamp, effective_fee, None)
    }

    pub fn transaction(&self) -> Cow<Transaction> {
        Cow::Borrowed(&self.transaction)
    }
}

impl BlockType for Block {
    type Transaction = Transaction;
    type AccountId = AccountIdentifier;
    type Tokens = Tokens;

    fn encode(self) -> EncodedBlock {
        EncodedBlock::from_vec(
            ProtoBuf::new(self)
                .into_bytes()
                .expect("unreachable: failed to encode a block"),
        )
    }

    fn decode(encoded_block: EncodedBlock) -> Result<Self, String> {
        Ok(ProtoBuf::from_bytes(encoded_block.into_vec())?.get())
    }

    fn block_hash(encoded_block: &EncodedBlock) -> HashOf<EncodedBlock> {
        let mut state = Sha256::new();
        state.write(encoded_block.as_slice());
        HashOf::new(state.finish())
    }

    fn parent_hash(&self) -> Option<HashOf<EncodedBlock>> {
        self.parent_hash
    }

    fn timestamp(&self) -> TimeStamp {
        self.timestamp
    }

    fn from_transaction(
        parent_hash: Option<HashOf<EncodedBlock>>,
        transaction: Self::Transaction,
        timestamp: TimeStamp,
        _effective_fee: Tokens,
        _fee_collector: Option<FeeCollector<AccountIdentifier>>,
    ) -> Self {
        Self {
            parent_hash,
            transaction,
            timestamp,
        }
    }
}

#[derive(Serialize, Deserialize, CandidType, Clone, Debug, PartialEq, Eq)]
pub struct TransferFee {
    /// The fee to pay to perform a transfer
    pub transfer_fee: Tokens,
}

impl Default for TransferFee {
    fn default() -> Self {
        TransferFee {
            transfer_fee: DEFAULT_TRANSFER_FEE,
        }
    }
}

#[allow(clippy::large_enum_variant)]
#[derive(Serialize, Deserialize, CandidType, Clone, Debug, PartialEq, Eq)]
pub enum LedgerCanisterPayload {
    Init(InitArgs),
    Upgrade(Option<UpgradeArgs>),
}

#[derive(Serialize, Deserialize, CandidType, Clone, Debug, PartialEq, Eq)]
pub struct LedgerCanisterInitPayload(pub LedgerCanisterPayload);

#[derive(Serialize, Deserialize, CandidType, Clone, Debug, PartialEq, Eq)]
pub struct LedgerCanisterUpgradePayload(pub LedgerCanisterPayload);

#[derive(Serialize, Deserialize, CandidType, Clone, Debug, PartialEq, Eq)]
pub struct UpgradeArgs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maximum_number_of_accounts: Option<usize>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icrc1_minting_account: Option<Account>,
}

// This is how we pass arguments to 'init' in main.rs
#[derive(Serialize, Deserialize, CandidType, Clone, Debug, PartialEq, Eq)]
pub struct InitArgs {
    pub minting_account: AccountIdentifier,
    pub icrc1_minting_account: Option<Account>,
    pub initial_values: HashMap<AccountIdentifier, Tokens>,
    pub max_message_size_bytes: Option<usize>,
    pub transaction_window: Option<Duration>,
    pub archive_options: Option<ArchiveOptions>,
    pub send_whitelist: HashSet<CanisterId>,
    pub transfer_fee: Option<Tokens>,
    pub token_symbol: Option<String>,
    pub token_name: Option<String>,
}

impl LedgerCanisterInitPayload {
    pub fn builder() -> LedgerCanisterInitPayloadBuilder {
        LedgerCanisterInitPayloadBuilder::new()
    }
    pub fn init_args(&mut self) -> Option<&mut InitArgs> {
        match &mut self.0 {
            LedgerCanisterPayload::Init(args) => Some(args),
            LedgerCanisterPayload::Upgrade(_) => None,
        }
    }
}

impl LedgerCanisterUpgradePayload {
    pub fn builder() -> LedgerCanisterUpgradePayloadBuilder {
        LedgerCanisterUpgradePayloadBuilder::new()
    }
}

pub struct LedgerCanisterInitPayloadBuilder {
    minting_account: Option<AccountIdentifier>,
    icrc1_minting_account: Option<Account>,
    initial_values: HashMap<AccountIdentifier, Tokens>,
    max_message_size_bytes: Option<usize>,
    transaction_window: Option<Duration>,
    archive_options: Option<ArchiveOptions>,
    send_whitelist: HashSet<CanisterId>,
    transfer_fee: Option<Tokens>,
    token_symbol: Option<String>,
    token_name: Option<String>,
}

impl LedgerCanisterInitPayloadBuilder {
    fn new() -> Self {
        Self {
            minting_account: None,
            icrc1_minting_account: None,
            initial_values: Default::default(),
            max_message_size_bytes: None,
            transaction_window: None,
            archive_options: None,
            send_whitelist: Default::default(),
            transfer_fee: None,
            token_symbol: None,
            token_name: None,
        }
    }

    pub fn minting_account(mut self, minting_account: AccountIdentifier) -> Self {
        self.minting_account = Some(minting_account);
        self
    }

    pub fn icrc1_minting_account(mut self, minting_account: Account) -> Self {
        self.icrc1_minting_account = Some(minting_account);
        self
    }

    pub fn initial_values(mut self, initial_values: HashMap<AccountIdentifier, Tokens>) -> Self {
        self.initial_values = initial_values;
        self
    }

    pub fn max_message_size_bytes(mut self, max_message_size_bytes: usize) -> Self {
        self.max_message_size_bytes = Some(max_message_size_bytes);
        self
    }

    pub fn transaction_window(mut self, transaction_window: Duration) -> Self {
        self.transaction_window = Some(transaction_window);
        self
    }

    pub fn archive_options(mut self, archive_options: ArchiveOptions) -> Self {
        self.archive_options = Some(archive_options);
        self
    }

    pub fn send_whitelist(mut self, send_whitelist: HashSet<CanisterId>) -> Self {
        self.send_whitelist = send_whitelist;
        self
    }

    pub fn transfer_fee(mut self, transfer_fee: Tokens) -> Self {
        self.transfer_fee = Some(transfer_fee);
        self
    }

    pub fn token_symbol_and_name(mut self, token_symbol: &str, token_name: &str) -> Self {
        self.token_symbol = Some(token_symbol.to_string());
        self.token_name = Some(token_name.to_string());
        self
    }

    pub fn build(self) -> Result<LedgerCanisterInitPayload, String> {
        let minting_account = self
            .minting_account
            .ok_or("minting_account must be set in the payload")?;

        // verify ledger's invariant about the maximum amount
        let mut sum = Tokens::ZERO;
        for initial_value in self.initial_values.values() {
            sum = sum
                .checked_add(initial_value)
                .ok_or_else(|| "initial_values sum overflows".to_string())?
        }

        // Don't allow self-transfers of the minting canister
        if self.initial_values.get(&minting_account).is_some() {
            return Err(
                "initial_values cannot contain transfers to the minting_account".to_string(),
            );
        }

        Ok(LedgerCanisterInitPayload(LedgerCanisterPayload::Init(
            InitArgs {
                minting_account,
                icrc1_minting_account: self.icrc1_minting_account,
                initial_values: self.initial_values,
                max_message_size_bytes: self.max_message_size_bytes,
                transaction_window: self.transaction_window,
                archive_options: self.archive_options,
                send_whitelist: self.send_whitelist,
                transfer_fee: self.transfer_fee,
                token_symbol: self.token_symbol,
                token_name: self.token_name,
            },
        )))
    }
}

pub struct LedgerCanisterUpgradePayloadBuilder {
    maximum_number_of_accounts: Option<usize>,
    icrc1_minting_account: Option<Account>,
}

impl LedgerCanisterUpgradePayloadBuilder {
    fn new() -> Self {
        Self {
            maximum_number_of_accounts: None,
            icrc1_minting_account: None,
        }
    }

    pub fn maximum_number_of_accounts(mut self, maximum_number_of_accounts: usize) -> Self {
        self.maximum_number_of_accounts = Some(maximum_number_of_accounts);
        self
    }

    pub fn icrc1_minting_account(mut self, minting_account: Account) -> Self {
        self.icrc1_minting_account = Some(minting_account);
        self
    }

    pub fn build(self) -> Result<LedgerCanisterUpgradePayload, String> {
        Ok(LedgerCanisterUpgradePayload(
            LedgerCanisterPayload::Upgrade(Some(UpgradeArgs {
                maximum_number_of_accounts: self.maximum_number_of_accounts,
                icrc1_minting_account: self.icrc1_minting_account,
            })),
        ))
    }
}

/// Argument taken by the send endpoint
#[derive(Serialize, Deserialize, CandidType, Clone, Hash, Debug, PartialEq, Eq)]
pub struct SendArgs {
    pub memo: Memo,
    pub amount: Tokens,
    pub fee: Tokens,
    pub from_subaccount: Option<Subaccount>,
    pub to: AccountIdentifier,
    pub created_at_time: Option<TimeStamp>,
}

impl From<SendArgs> for TransferArgs {
    fn from(
        SendArgs {
            memo,
            amount,
            fee,
            from_subaccount,
            to,
            created_at_time,
        }: SendArgs,
    ) -> Self {
        Self {
            memo,
            amount,
            fee,
            from_subaccount,
            to: to.to_address(),
            created_at_time,
        }
    }
}

pub type AccountIdBlob = [u8; 32];

/// Argument taken by the transfer endpoint
#[derive(Serialize, Deserialize, CandidType, Clone, Hash, Debug, PartialEq, Eq)]
pub struct TransferArgs {
    pub memo: Memo,
    pub amount: Tokens,
    pub fee: Tokens,
    pub from_subaccount: Option<Subaccount>,
    pub to: AccountIdBlob,
    pub created_at_time: Option<TimeStamp>,
}

#[derive(Serialize, Deserialize, CandidType, Clone, Debug, PartialEq, Eq)]
pub enum TransferError {
    BadFee { expected_fee: Tokens },
    InsufficientFunds { balance: Tokens },
    TxTooOld { allowed_window_nanos: u64 },
    TxCreatedInFuture,
    TxDuplicate { duplicate_of: BlockIndex },
}

impl fmt::Display for TransferError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadFee { expected_fee } => {
                write!(f, "transfer fee should be {}", expected_fee)
            }
            Self::InsufficientFunds { balance } => {
                write!(
                    f,
                    "the debit account doesn't have enough funds to complete the transaction, current balance: {}",
                    balance
                )
            }
            Self::TxTooOld {
                allowed_window_nanos,
            } => write!(
                f,
                "transaction is older than {} seconds",
                allowed_window_nanos / 1_000_000_000
            ),
            Self::TxCreatedInFuture => write!(f, "transaction's created_at_time is in future"),
            Self::TxDuplicate { duplicate_of } => write!(
                f,
                "transaction is a duplicate of another transaction in block {}",
                duplicate_of
            ),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum PaymentError {
    Reject(String),
    TransferError(TransferError),
}

/// Struct sent by the ledger canister when it notifies a recipient of a payment
#[derive(Serialize, Deserialize, CandidType, Clone, Hash, Debug, PartialEq, Eq)]
pub struct TransactionNotification {
    pub from: PrincipalId,
    pub from_subaccount: Option<Subaccount>,
    pub to: CanisterId,
    pub to_subaccount: Option<Subaccount>,
    pub block_height: BlockIndex,
    pub amount: Tokens,
    pub memo: Memo,
}

/// Argument taken by the notification endpoint
#[derive(Serialize, Deserialize, CandidType, Clone, Hash, Debug, PartialEq, Eq)]
pub struct NotifyCanisterArgs {
    pub block_height: BlockIndex,
    pub max_fee: Tokens,
    pub from_subaccount: Option<Subaccount>,
    pub to_canister: CanisterId,
    pub to_subaccount: Option<Subaccount>,
}

impl NotifyCanisterArgs {
    /// Construct a `notify` call to notify a canister about the
    /// transaction created by a previous `send` call. `block_height`
    /// is the index of the block returned by `send`.
    pub fn new_from_send(
        send_args: &SendArgs,
        block_height: BlockIndex,
        to_canister: CanisterId,
        to_subaccount: Option<Subaccount>,
    ) -> Result<Self, String> {
        if AccountIdentifier::new(to_canister.get(), to_subaccount) != send_args.to {
            Err("Account identifier does not match canister args".to_string())
        } else {
            Ok(NotifyCanisterArgs {
                block_height,

                max_fee: send_args.fee,
                from_subaccount: send_args.from_subaccount,
                to_canister,
                to_subaccount,
            })
        }
    }
}

/// Arguments taken by the account_balance candid endpoint.
#[derive(Serialize, Deserialize, CandidType, Clone, Hash, Debug, PartialEq, Eq)]
pub struct BinaryAccountBalanceArgs {
    pub account: AccountIdBlob,
}

/// Argument taken by the account_balance_dfx endpoint
#[derive(Serialize, Deserialize, CandidType, Clone, Hash, Debug, PartialEq, Eq)]
pub struct AccountBalanceArgs {
    pub account: AccountIdentifier,
}

impl AccountBalanceArgs {
    pub fn new(account: AccountIdentifier) -> Self {
        AccountBalanceArgs { account }
    }
}

/// An operation which modifies account balances
#[derive(
    Serialize, Deserialize, CandidType, Clone, Hash, Debug, PartialEq, Eq, PartialOrd, Ord,
)]
pub enum CandidOperation {
    Burn {
        from: AccountIdBlob,
        amount: Tokens,
    },
    Mint {
        to: AccountIdBlob,
        amount: Tokens,
    },
    Transfer {
        from: AccountIdBlob,
        to: AccountIdBlob,
        amount: Tokens,
        fee: Tokens,
    },
    Approve {
        from: AccountIdBlob,
        spender: AccountIdBlob,
        // This field is deprecated and should not be used.
        allowance_e8s: i128,
        allowance: Tokens,
        fee: Tokens,
        expires_at: Option<TimeStamp>,
    },
    TransferFrom {
        from: AccountIdBlob,
        to: AccountIdBlob,
        spender: AccountIdBlob,
        amount: Tokens,
        fee: Tokens,
    },
}

impl From<Operation> for CandidOperation {
    fn from(op: Operation) -> Self {
        match op {
            Operation::Burn { from, amount } => Self::Burn {
                from: from.to_address(),
                amount,
            },
            Operation::Mint { to, amount } => Self::Mint {
                to: to.to_address(),
                amount,
            },
            Operation::Transfer {
                from,
                to,
                amount,
                fee,
            } => Self::Transfer {
                from: from.to_address(),
                to: to.to_address(),
                amount,
                fee,
            },
            Operation::Approve {
                from,
                spender,
                allowance,
                fee,
                expires_at,
            } => Self::Approve {
                from: from.to_address(),
                spender: spender.to_address(),
                allowance_e8s: allowance.get_e8s() as i128,
                fee,
                expires_at,
                allowance,
            },
            Operation::TransferFrom {
                from,
                to,
                spender,
                amount,
                fee,
            } => Self::TransferFrom {
                from: from.to_address(),
                to: to.to_address(),
                spender: spender.to_address(),
                amount,
                fee,
            },
        }
    }
}

impl TryFrom<CandidOperation> for Operation {
    type Error = String;

    fn try_from(value: CandidOperation) -> Result<Self, Self::Error> {
        let address_to_accountidentifier = |acc| -> Result<AccountIdentifier, Self::Error> {
            AccountIdentifier::from_address(acc).map_err(|err| err.to_string())
        };
        Ok(match value {
            CandidOperation::Burn { from, amount } => Operation::Burn {
                from: address_to_accountidentifier(from)?,
                amount,
            },
            CandidOperation::Mint { to, amount } => Operation::Mint {
                to: address_to_accountidentifier(to)?,
                amount,
            },
            CandidOperation::Transfer {
                from,
                to,
                amount,
                fee,
            } => Operation::Transfer {
                to: address_to_accountidentifier(to)?,
                from: address_to_accountidentifier(from)?,
                amount,
                fee,
            },
            CandidOperation::Approve {
                from,
                spender,
                fee,
                expires_at,
                allowance,
                ..
            } => Operation::Approve {
                spender: address_to_accountidentifier(spender)?,
                from: address_to_accountidentifier(from)?,
                allowance,
                fee,
                expires_at,
            },
            CandidOperation::TransferFrom {
                from,
                to,
                spender,
                amount,
                fee,
            } => Operation::TransferFrom {
                spender: address_to_accountidentifier(spender)?,
                from: address_to_accountidentifier(from)?,
                to: address_to_accountidentifier(to)?,
                amount,
                fee,
            },
        })
    }
}

/// An operation with the metadata the client generated attached to it
#[derive(
    Serialize, Deserialize, CandidType, Clone, Hash, Debug, PartialEq, Eq, PartialOrd, Ord,
)]
pub struct CandidTransaction {
    pub operation: Option<CandidOperation>,
    pub memo: Memo,
    pub icrc1_memo: Option<ByteBuf>,
    pub created_at_time: TimeStamp,
}

impl TryFrom<CandidTransaction> for Transaction {
    type Error = String;
    fn try_from(value: CandidTransaction) -> Result<Self, Self::Error> {
        Ok(Self {
            operation: value.operation.map_or(
                Err("Operation is None --> Cannot convert CandidOperation to icp_ledger Operation"),
                |candid_block| Ok(Operation::try_from(candid_block)),
            )??,
            memo: value.memo,
            created_at_time: Some(value.created_at_time),
            icrc1_memo: value.icrc1_memo,
        })
    }
}

#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct CandidBlock {
    pub parent_hash: Option<[u8; HASH_LENGTH]>,
    pub transaction: CandidTransaction,
    pub timestamp: TimeStamp,
}

impl From<Block> for CandidBlock {
    fn from(
        Block {
            parent_hash,
            transaction,
            timestamp,
        }: Block,
    ) -> Self {
        Self {
            parent_hash: parent_hash.map(|h| h.into_bytes()),
            transaction: CandidTransaction {
                memo: transaction.memo,
                icrc1_memo: transaction.icrc1_memo,
                operation: Some(transaction.operation.into()),
                created_at_time: transaction.created_at_time.unwrap_or(timestamp),
            },
            timestamp,
        }
    }
}

impl TryFrom<CandidBlock> for Block {
    type Error = String;
    fn try_from(value: CandidBlock) -> Result<Self, Self::Error> {
        Ok(Self {
            parent_hash: value.parent_hash.map(HashOf::<EncodedBlock>::new),
            transaction: Transaction::try_from(value.transaction)?,
            timestamp: value.timestamp,
        })
    }
}

/// Argument taken by the transfer fee endpoint
///
/// The reason it is a struct is so that it can be extended -- e.g., to be able
/// to query past values. Requiring 1 candid value instead of zero is a
/// non-backward compatible change. But adding optional fields to a struct taken
/// as input is backward-compatible.
#[derive(Serialize, Deserialize, CandidType, Clone, Hash, Debug, PartialEq, Eq)]
pub struct TransferFeeArgs {}

/// Argument taken by the total_supply endpoint
///
/// The reason it is a struct is so that it can be extended -- e.g., to be able
/// to query past values. Requiring 1 candid value instead of zero is a
/// non-backward compatible change. But adding optional fields to a struct taken
/// as input is backward-compatible.
#[derive(Serialize, Deserialize, CandidType, Clone, Hash, Debug, PartialEq, Eq)]
pub struct TotalSupplyArgs {}

#[derive(Serialize, Deserialize, CandidType, Clone, Hash, Debug, PartialEq, Eq)]
pub struct Symbol {
    pub symbol: String,
}

#[derive(Serialize, Deserialize, CandidType, Clone, Hash, Debug, PartialEq, Eq)]
pub struct Name {
    pub name: String,
}

#[derive(Serialize, Deserialize, CandidType, Clone, Hash, Debug, PartialEq, Eq)]
pub struct Decimals {
    pub decimals: u32,
}

#[derive(Serialize, Deserialize, CandidType, Clone, Hash, Debug, PartialEq, Eq)]
pub struct ArchiveInfo {
    pub canister_id: CanisterId,
}

#[derive(Serialize, Deserialize, CandidType, Clone, Hash, Debug, PartialEq, Eq)]
pub struct Archives {
    pub archives: Vec<ArchiveInfo>,
}

/// Argument returned by the tip_of_chain endpoint
pub struct TipOfChainRes {
    pub certification: Option<Vec<u8>>,
    pub tip_index: BlockIndex,
}

#[derive(Serialize, Deserialize, CandidType, Debug, Clone)]
pub struct GetBlocksArgs {
    pub start: BlockIndex,
    pub length: usize,
}

#[derive(Serialize, Deserialize, CandidType, Debug, Clone)]
pub struct BlockRange {
    pub blocks: Vec<CandidBlock>,
}

pub type GetBlocksResult = Result<BlockRange, GetBlocksError>;

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize, CandidType, Clone)]
pub enum GetBlocksError {
    BadFirstBlockIndex {
        requested_index: BlockIndex,
        first_valid_index: BlockIndex,
    },
    Other {
        error_code: u64,
        error_message: String,
    },
}

pub struct GetBlocksRes(pub Result<Vec<EncodedBlock>, String>);

pub struct IterBlocksArgs {
    pub start: usize,
    pub length: usize,
}

impl IterBlocksArgs {
    pub fn new(start: usize, length: usize) -> Self {
        IterBlocksArgs { start, length }
    }
}

pub struct IterBlocksRes(pub Vec<EncodedBlock>);

// These is going away soon
pub struct BlockArg(pub BlockIndex);
pub struct BlockRes(pub Option<Result<EncodedBlock, CanisterId>>);

// A helper function for ledger/get_blocks and archive_node/get_blocks endpoints
pub fn get_blocks(
    blocks: &[EncodedBlock],
    range_from_offset: BlockIndex,
    range_from: BlockIndex,
    length: usize,
) -> GetBlocksRes {
    // Inclusive end of the range of *requested* blocks
    let requested_range_to = range_from as usize + length - 1;
    // Inclusive end of the range of *available* blocks
    let range_to = range_from_offset as usize + blocks.len() - 1;
    // Example: If the Node stores 10 blocks beginning at BlockIndex 100, i.e.
    // [100 .. 109] then requesting blocks at BlockIndex < 100 or BlockIndex
    // > 109 is an error
    if range_from < range_from_offset || requested_range_to > range_to {
        return GetBlocksRes(Err(format!("Requested blocks outside the range stored in the archive node. Requested [{} .. {}]. Available [{} .. {}].",
            range_from, requested_range_to, range_from_offset, range_to)));
    }
    // Example: If the node stores blocks [100 .. 109] then BLOCK_HEIGHT_OFFSET
    // is 100 and the Block with BlockIndex 100 is at index 0
    let offset = (range_from - range_from_offset) as usize;
    GetBlocksRes(Ok(blocks[offset..offset + length].to_vec()))
}

// A helper function for ledger/iter_blocks and archive_node/iter_blocks
// endpoints
pub fn iter_blocks(blocks: &[EncodedBlock], offset: usize, length: usize) -> IterBlocksRes {
    let start = std::cmp::min(offset, blocks.len());
    let end = std::cmp::min(start + length, blocks.len());
    let blocks = blocks[start..end].to_vec();
    IterBlocksRes(blocks)
}

#[derive(CandidType, Deserialize, Clone)]
pub enum CyclesResponse {
    CanisterCreated(CanisterId),
    // Silly requirement by the candid derivation
    ToppedUp(()),
    Refunded(String, Option<BlockIndex>),
}

#[derive(Debug, CandidType, Deserialize, Clone)]
pub struct ArchivedBlocksRange {
    pub start: BlockIndex,
    pub length: u64,
    pub callback: QueryArchiveBlocksFn,
}

#[derive(Debug, CandidType, Deserialize, Clone)]
pub struct QueryBlocksResponse {
    pub chain_length: u64,
    pub certificate: Option<serde_bytes::ByteBuf>,
    pub blocks: Vec<CandidBlock>,
    pub first_block_index: BlockIndex,
    pub archived_blocks: Vec<ArchivedBlocksRange>,
}

#[derive(Debug, CandidType, Deserialize, Clone)]
pub struct QueryEncodedBlocksResponse {
    pub chain_length: u64,
    pub certificate: Option<serde_bytes::ByteBuf>,
    pub blocks: Vec<EncodedBlock>,
    pub first_block_index: BlockIndex,
    pub archived_blocks: Vec<ArchivedEncodedBlocksRange>,
}

pub type GetEncodedBlocksResult = Result<Vec<EncodedBlock>, GetBlocksError>;

#[derive(Debug, CandidType, Deserialize, Clone)]
pub struct ArchivedEncodedBlocksRange {
    pub start: BlockIndex,
    pub length: u64,
    pub callback: QueryArchiveEncodedBlocksFn,
}

pub type QueryArchiveBlocksFn =
    icrc_ledger_types::icrc3::archive::QueryArchiveFn<GetBlocksArgs, GetBlocksResult>;
pub type QueryArchiveEncodedBlocksFn =
    icrc_ledger_types::icrc3::archive::QueryArchiveFn<GetBlocksArgs, GetEncodedBlocksResult>;
