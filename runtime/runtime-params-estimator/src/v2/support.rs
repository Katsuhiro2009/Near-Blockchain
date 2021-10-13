use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;
use std::{fmt, ops};

use near_crypto::{InMemorySigner, KeyType};
use near_primitives::hash::CryptoHash;
use near_primitives::transaction::{
    Action, DeployContractAction, FunctionCallAction, SignedTransaction,
};
use near_primitives::types::{AccountId, Gas};
use near_vm_logic::ExtCosts;
use num_rational::Ratio;
use rand::prelude::ThreadRng;
use rand::Rng;

use crate::cases::ratio_to_gas;
use crate::testbed::RuntimeTestbed;
use crate::testbed_runners::{end_count, get_account_id, start_count, Config, GasMetric};

#[derive(Default)]
pub(crate) struct CachedCosts {
    pub(crate) action_receipt_creation: Option<GasCost>,
    pub(crate) action_sir_receipt_creation: Option<GasCost>,
    pub(crate) action_add_function_access_key_base: Option<GasCost>,
    pub(crate) action_deploy_contract_base: Option<GasCost>,
    pub(crate) noop_host_function_call_cost: Option<GasCost>,
    pub(crate) action_function_call_base_per_byte_v2: Option<(GasCost, GasCost)>,
}

/// Global context shared by all cost calculating functions.
pub(crate) struct Ctx<'c> {
    pub(crate) config: &'c Config,
    pub(crate) cached: CachedCosts,
    contracts_testbed: Option<ContractTestbedProto>,
}

struct ContractTestbedProto {
    accounts: Vec<AccountId>,
    state_dump: tempfile::TempDir,
    nonces: HashMap<AccountId, u64>,
}

impl<'c> Ctx<'c> {
    pub(crate) fn new(config: &'c Config) -> Self {
        let cached = CachedCosts::default();
        Self { cached, config, contracts_testbed: None }
    }

    pub(crate) fn test_bed(&mut self) -> TestBed<'_> {
        let inner = RuntimeTestbed::from_state_dump(&self.config.state_dump_path);
        TestBed {
            config: &self.config,
            inner,
            transaction_builder: TransactionBuilder {
                accounts: (0..self.config.active_accounts).map(get_account_id).collect(),
                nonces: HashMap::new(),
                used_accounts: HashSet::new(),
            },
        }
    }

    pub(crate) fn test_bed_with_contracts(&mut self) -> TestBed<'_> {
        if self.contracts_testbed.is_none() {
            let code = self.read_resource(if cfg!(feature = "nightly_protocol_features") {
                "test-contract/res/nightly_small_contract.wasm"
            } else {
                "test-contract/res/stable_small_contract.wasm"
            });

            let mut tb = self.test_bed();
            let accounts = deploy_contracts(&mut tb, code);
            tb.inner.dump_state().unwrap();

            self.contracts_testbed = Some(ContractTestbedProto {
                accounts,
                state_dump: tb.inner.workdir,
                nonces: tb.transaction_builder.nonces,
            });
        }
        let proto = self.contracts_testbed.as_ref().unwrap();

        let inner = RuntimeTestbed::from_state_dump(proto.state_dump.path());
        TestBed {
            config: &self.config,
            inner,
            transaction_builder: TransactionBuilder {
                accounts: proto.accounts.clone(),
                nonces: proto.nonces.clone(),
                used_accounts: HashSet::new(),
            },
        }
    }

    pub(crate) fn read_resource(&mut self, path: &str) -> Vec<u8> {
        let dir = env!("CARGO_MANIFEST_DIR");
        let path = Path::new(dir).join(path);
        std::fs::read(&path).unwrap_or_else(|err| {
            panic!("failed to load test resource: {}, {}", path.display(), err)
        })
    }
}

fn deploy_contracts(test_bed: &mut TestBed, code: Vec<u8>) -> Vec<AccountId> {
    let mut accounts_with_code = Vec::new();
    for _ in 0..3 {
        let block_size = 100;
        let n_blocks = test_bed.config.warmup_iters_per_block + test_bed.config.iter_per_block;
        let blocks = {
            let mut blocks = Vec::with_capacity(n_blocks);
            for _ in 0..n_blocks {
                let mut block = Vec::with_capacity(block_size);
                for _ in 0..block_size {
                    let tb = test_bed.transaction_builder();
                    let sender = tb.random_unused_account();
                    let receiver = sender.clone();

                    accounts_with_code.push(sender.clone());

                    let actions =
                        vec![Action::DeployContract(DeployContractAction { code: code.clone() })];
                    let tx = tb.transaction_from_actions(sender, receiver, actions);
                    block.push(tx);
                }
                blocks.push(block);
            }
            blocks
        };
        test_bed.measure_blocks(blocks);
    }
    accounts_with_code
}

/// A single isolated instance of near.
///
/// We use it to time processing a bunch of blocks.
pub(crate) struct TestBed<'c> {
    pub(crate) config: &'c Config,
    inner: RuntimeTestbed,
    transaction_builder: TransactionBuilder,
}

impl<'c> TestBed<'c> {
    pub(crate) fn transaction_builder(&mut self) -> &mut TransactionBuilder {
        &mut self.transaction_builder
    }

    pub(crate) fn measure_blocks<'a>(
        &'a mut self,
        blocks: Vec<Vec<SignedTransaction>>,
    ) -> Vec<(GasCost, HashMap<ExtCosts, u64>)> {
        let allow_failures = false;

        let mut res = Vec::with_capacity(blocks.len());

        for block in blocks {
            node_runtime::with_ext_cost_counter(|cc| cc.clear());
            let start = start_count(self.config.metric);
            self.inner.process_block(&block, allow_failures);
            self.inner.process_blocks_until_no_receipts(allow_failures);
            let measured = end_count(self.config.metric, &start);

            let gas_cost = GasCost { value: measured.into(), metric: self.config.metric };

            let mut ext_costs: HashMap<ExtCosts, u64> = HashMap::new();
            node_runtime::with_ext_cost_counter(|cc| {
                for (c, v) in cc.drain() {
                    ext_costs.insert(c, v);
                }
            });
            res.push((gas_cost, ext_costs));
        }

        res
    }
}

/// A helper to create transaction for processing by a `TestBed`.
pub(crate) struct TransactionBuilder {
    accounts: Vec<AccountId>,
    nonces: HashMap<AccountId, u64>,
    used_accounts: HashSet<AccountId>,
}

impl TransactionBuilder {
    pub(crate) fn transaction_from_actions(
        &mut self,
        sender: AccountId,
        receiver: AccountId,
        actions: Vec<Action>,
    ) -> SignedTransaction {
        let signer = InMemorySigner::from_seed(sender.clone(), KeyType::ED25519, sender.as_ref());
        let nonce = self.nonce(&sender);

        SignedTransaction::from_actions(
            nonce as u64,
            sender.clone(),
            receiver,
            &signer,
            actions,
            CryptoHash::default(),
        )
    }

    pub(crate) fn transaction_from_function_call(
        &mut self,
        sender: AccountId,
        method: &str,
        args: Vec<u8>,
    ) -> SignedTransaction {
        let receiver = sender.clone();
        let actions = vec![Action::FunctionCall(FunctionCallAction {
            method_name: method.to_string(),
            args,
            gas: 10u64.pow(18),
            deposit: 0,
        })];
        self.transaction_from_actions(sender, receiver, actions)
    }

    pub(crate) fn rng(&mut self) -> ThreadRng {
        rand::thread_rng()
    }

    pub(crate) fn account(&mut self, account_index: usize) -> AccountId {
        get_account_id(account_index)
    }
    pub(crate) fn random_account(&mut self) -> AccountId {
        let account_index = self.rng().gen_range(0, self.accounts.len());
        self.accounts[account_index].clone()
    }
    pub(crate) fn random_unused_account(&mut self) -> AccountId {
        loop {
            let account = self.random_account();
            if self.used_accounts.insert(account.clone()) {
                return account;
            }
        }
    }
    pub(crate) fn random_account_pair(&mut self) -> (AccountId, AccountId) {
        let first = self.random_account();
        loop {
            let second = self.random_account();
            if first != second {
                return (first, second);
            }
        }
    }
    fn nonce(&mut self, account_id: &AccountId) -> u64 {
        let nonce = self.nonces.entry(account_id.clone()).or_default();
        *nonce += 1;
        *nonce
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct GasCost {
    /// The smallest thing we are measuring is one wasm instruction, and it
    /// takes about a nanosecond, so we do need to account for fractional
    /// nanoseconds here!
    pub value: Ratio<u64>,
    pub metric: GasMetric,
}

impl fmt::Debug for GasCost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.metric {
            GasMetric::ICount => write!(f, "{}i", self.value),
            GasMetric::Time => fmt::Debug::fmt(&Duration::from_nanos(self.value.to_integer()), f),
        }
    }
}

impl ops::Add for GasCost {
    type Output = GasCost;

    fn add(self, rhs: GasCost) -> Self::Output {
        assert_eq!(self.metric, rhs.metric);
        GasCost { value: self.value + rhs.value, metric: self.metric }
    }
}

impl ops::AddAssign for GasCost {
    fn add_assign(&mut self, rhs: GasCost) {
        *self = self.clone() + rhs;
    }
}

impl ops::Sub for GasCost {
    type Output = GasCost;

    fn sub(self, rhs: GasCost) -> Self::Output {
        assert_eq!(self.metric, rhs.metric);
        GasCost { value: self.value - rhs.value, metric: self.metric }
    }
}

impl ops::Div<u64> for GasCost {
    type Output = GasCost;

    fn div(self, rhs: u64) -> Self::Output {
        GasCost { value: self.value / rhs, metric: self.metric }
    }
}

impl PartialOrd for GasCost {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for GasCost {
    fn cmp(&self, other: &Self) -> Ordering {
        self.value.cmp(&other.value)
    }
}

impl GasCost {
    pub(crate) fn to_gas(self) -> Gas {
        ratio_to_gas(self.metric, self.value)
    }
}