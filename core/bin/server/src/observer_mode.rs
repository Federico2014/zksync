//! Observer mode continuously checks the database and keeps updated state of the accounts in memory.
//! The state is then fed to other actors when server transitions to the leader mode.

use crate::state_keeper::ZkSyncStateInitParams;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use zksync_circuit::witness::{
    ChangePubkeyOffChainWitness, CloseAccountWitness, DepositWitness, ForcedExitWitness,
    FullExitWitness, TransferToNewWitness, TransferWitness, WithdrawWitness, Witness,
};
use zksync_crypto::circuit::account::CircuitAccount;
use zksync_crypto::circuit::CircuitAccountTree;
use zksync_types::{BlockNumber, ZkSyncOp};

/// The state being observed during observer mode. Meant to be used later to initialize server actors.
pub struct ObservedState {
    /// Used to initialize `ZkSyncStateKeeper`
    pub state_keeper_init: ZkSyncStateInitParams,
    /// Used to initialize pool of prover_server.
    pub circuit_acc_tree: CircuitAccountTree,
    /// Block number corresponding to the state in `circuit_acc_tree`.
    pub circuit_tree_block: BlockNumber,

    pub connection_pool: zksync_storage::ConnectionPool,
}

impl ObservedState {
    fn new(connection_pool: zksync_storage::ConnectionPool) -> Self {
        Self {
            state_keeper_init: ZkSyncStateInitParams::new(),
            circuit_acc_tree: CircuitAccountTree::new(zksync_crypto::params::account_tree_depth()),
            circuit_tree_block: 0,
            connection_pool,
        }
    }

    /// Init state by pulling verified and committed state from db.
    async fn init(&mut self) -> Result<(), anyhow::Error> {
        self.init_circuit_tree().await?;
        log::info!("updated circuit tree to block: {}", self.circuit_tree_block);
        let mut storage = self.connection_pool.access_storage().await?;
        self.state_keeper_init = ZkSyncStateInitParams::restore_from_db(&mut storage).await?;
        log::info!(
            "updated state keeper init params to block: {}",
            self.state_keeper_init.last_block_number
        );
        Ok(())
    }

    async fn init_circuit_tree(&mut self) -> Result<(), anyhow::Error> {
        let mut storage = self.connection_pool.access_storage().await?;

        let (block_number, accounts) =
            storage
                .chain()
                .state_schema()
                .load_verified_state()
                .await
                .map_err(|e| anyhow::format_err!("couldn't load committed state: {}", e))?;
        for (account_id, account) in accounts.into_iter() {
            let circuit_account = CircuitAccount::from(account.clone());
            self.circuit_acc_tree.insert(account_id, circuit_account);
        }
        self.circuit_tree_block = block_number;
        Ok(())
    }

    /// Pulls new changes from db and update.
    async fn update(&mut self) -> Result<(), anyhow::Error> {
        let old = self.circuit_tree_block;
        self.update_circuit_account_tree().await?;
        if old != self.circuit_tree_block {
            log::info!("updated circuit tree to block: {}", self.circuit_tree_block);
        }
        let old = self.state_keeper_init.last_block_number;

        let mut storage = self.connection_pool.access_storage().await?;
        self.state_keeper_init.load_state_diff(&mut storage).await?;
        if old != self.state_keeper_init.last_block_number {
            log::info!(
                "updated state keeper init params to block: {}",
                self.state_keeper_init.last_block_number
            );
        }
        Ok(())
    }

    async fn update_circuit_account_tree(&mut self) -> Result<(), anyhow::Error> {
        let block_number = {
            let mut storage = self.connection_pool.access_storage().await?;
            storage
                .chain()
                .block_schema()
                .get_last_verified_block()
                .await
                .map_err(|e| anyhow::format_err!("failed to get last committed block: {}", e))?
        };

        for bn in self.circuit_tree_block..block_number {
            let ops = {
                let mut storage = self.connection_pool.access_storage().await?;
                storage
                    .chain()
                    .block_schema()
                    .get_block_operations(bn + 1)
                    .await
                    .map_err(|e| anyhow::format_err!("failed to get block operations {}", e))?
            };
            self.apply(ops);
        }
        self.circuit_tree_block = block_number;
        Ok(())
    }

    fn apply(&mut self, ops: Vec<ZkSyncOp>) {
        for op in ops {
            match op {
                ZkSyncOp::Deposit(deposit) => {
                    DepositWitness::apply_tx(&mut self.circuit_acc_tree, &deposit);
                }
                ZkSyncOp::Transfer(transfer) => {
                    TransferWitness::apply_tx(&mut self.circuit_acc_tree, &transfer);
                }
                ZkSyncOp::TransferToNew(transfer_to_new) => {
                    TransferToNewWitness::apply_tx(&mut self.circuit_acc_tree, &transfer_to_new);
                }
                ZkSyncOp::Withdraw(withdraw) => {
                    WithdrawWitness::apply_tx(&mut self.circuit_acc_tree, &withdraw);
                }
                ZkSyncOp::Close(close) => {
                    CloseAccountWitness::apply_tx(&mut self.circuit_acc_tree, &close);
                }
                ZkSyncOp::FullExit(full_exit) => {
                    let success = full_exit.withdraw_amount.is_some();
                    FullExitWitness::apply_tx(&mut self.circuit_acc_tree, &(*full_exit, success));
                }
                ZkSyncOp::ChangePubKeyOffchain(change_pubkey) => {
                    ChangePubkeyOffChainWitness::apply_tx(
                        &mut self.circuit_acc_tree,
                        &change_pubkey,
                    );
                }
                ZkSyncOp::ForcedExit(forced_exit) => {
                    ForcedExitWitness::apply_tx(&mut self.circuit_acc_tree, &forced_exit);
                }
                ZkSyncOp::Noop(_) => {}
            }
        }
    }
}

/// Accumulate state from db continuously and return that state on stop signal.
///
/// # Panics
/// Panics on failed connection to db.
pub async fn run(
    conn_pool: zksync_storage::ConnectionPool,
    interval: Duration,
    stop: mpsc::Receiver<()>,
) -> ObservedState {
    log::info!("starting observer mode");
    let mut observed_state = ObservedState::new(conn_pool);
    observed_state
        .init()
        .await
        .expect("failed to init observed state");
    loop {
        let exit = match stop.try_recv() {
            Err(mpsc::TryRecvError::Empty) => false,
            Err(e) => {
                panic!("stop channel recv error: {}", e);
            }
            Ok(_) => true,
        };
        thread::sleep(interval);
        observed_state
            .update()
            .await
            .expect("failed to update observed state");
        if exit {
            break;
        }
    }
    observed_state
}
