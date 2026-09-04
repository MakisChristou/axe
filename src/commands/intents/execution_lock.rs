use std::fs::{File, OpenOptions, TryLockError};

use alloy::primitives::Address;
use eyre::{Result, WrapErr, eyre};

pub(super) struct ExecutionLock {
    _file: File,
}

impl ExecutionLock {
    pub fn acquire(wallet: Address) -> Result<Self> {
        let path = std::env::temp_dir().join(format!("axe-intents-{wallet}.lock"));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .wrap_err_with(|| format!("could not open intent execution lock {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(TryLockError::WouldBlock) => Err(eyre!(
                "another axe intent execution command is using wallet {wallet}; wait for it to finish"
            )),
            Err(TryLockError::Error(error)) => {
                Err(error).wrap_err_with(|| format!("could not lock intent wallet {wallet}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prevents_two_execution_commands_using_the_same_wallet() {
        let mut bytes = [0u8; 20];
        bytes[16..].copy_from_slice(&std::process::id().to_be_bytes());
        let wallet = Address::from(bytes);
        let first = ExecutionLock::acquire(wallet).unwrap();

        assert!(ExecutionLock::acquire(wallet).is_err());
        drop(first);
        assert!(ExecutionLock::acquire(wallet).is_ok());
    }
}
