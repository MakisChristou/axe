//! Shared handling for independently spawned load-test tasks.

use eyre::Result;
use futures::future;
use tokio::task::JoinHandle;

/// Await every task and surface the first join failure after all tasks settle.
///
/// Dropping a `JoinHandle` detaches its task, so using `try_join_all` would let
/// the remaining senders continue invisibly after one panic. Waiting for the
/// complete group keeps command completion aligned with actual task lifetime.
pub(super) async fn join_all<T>(tasks: Vec<JoinHandle<T>>) -> Result<Vec<T>> {
    future::join_all(tasks)
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::join_all;

    #[tokio::test]
    async fn propagates_panics_after_awaiting_the_group() {
        let tasks = vec![
            tokio::spawn(async { 1 }),
            tokio::spawn(async { panic!("sender panic") }),
        ];

        assert!(join_all(tasks).await.is_err());
    }
}
