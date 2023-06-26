//! Executor Factory

use crate::{change::BundleState, StateProvider};
use reth_interfaces::executor::BlockExecutionError;
use reth_primitives::{Address, Block, ChainSpec, U256};

/// Executor factory that would create the EVM with particular state provider.
///
/// It can be used to mock executor.
pub trait ExecutorFactory: Send + Sync + 'static {
    /// Executor with [`StateProvider`]
    fn with_sp<'a, SP: StateProvider + 'a>(&'a self, _sp: SP) -> Box<dyn BlockExecutor + 'a>;

    /// Return internal chainspec
    fn chain_spec(&self) -> &ChainSpec;
}

/// An executor capable of executing a block.
pub trait BlockExecutor {
    /// Execute a block.
    ///
    /// The number of `senders` should be equal to the number of transactions in the block.
    ///
    /// If no senders are specified, the `execute` function MUST recover the senders for the
    /// provided block's transactions internally. We use this to allow for calculating senders in
    /// parallel in e.g. staged sync, so that execution can happen without paying for sender
    /// recovery costs.
    fn execute(
        &mut self,
        block: &Block,
        total_difficulty: U256,
        senders: Option<Vec<Address>>,
    ) -> Result<(), BlockExecutionError>;

    /// Executes the block and checks receipts
    fn execute_and_verify_receipt(
        &mut self,
        block: &Block,
        total_difficulty: U256,
        senders: Option<Vec<Address>>,
    ) -> Result<(), BlockExecutionError>;

    /// Return bundle state. This is output of the execution.
    fn take_output_state(&mut self) -> BundleState;
}
