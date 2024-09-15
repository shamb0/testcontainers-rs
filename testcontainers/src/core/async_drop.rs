use std::sync::OnceLock;

use futures::future::BoxFuture;

static DROP_TASK_SENDER: OnceLock<tokio::sync::mpsc::UnboundedSender<BoxFuture<'static, ()>>> =
    OnceLock::new();

/// A helper to perform async operations in `Drop` implementation.
///
/// The behavior depends on the runtime flavor used in the test:
/// - `multi-threaded` runtime: it will use `tokio::task::block_in_place` to run the provided future
/// - `current-thread` runtime: it spawns a separate tokio runtime in a dedicated thread to run the provided futures.
///     * Only 1 drop-worker for the process, regardless of number of containers and drops.
// We can consider creating `AsyncDrop` trait + `AsyncDropGuard<T: AsyncDrop>` wrapper to make it more ergonomic.
// However, we have a only couple of places where we need this functionality.
pub(crate) fn async_drop(future: impl std::future::Future<Output = ()> + Send + 'static) {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            match handle.runtime_flavor() {
                tokio::runtime::RuntimeFlavor::CurrentThread => {
                    let (tx, rx) = std::sync::mpsc::sync_channel(1);
                    dropper_task_sender()
                        .send(Box::pin(async move {
                            future.await;
                            let _ = tx.send(());
                        }))
                        .expect("drop-worker must be running: failed to send drop task");
                    let _ = rx.recv();
                }
                tokio::runtime::RuntimeFlavor::MultiThread => {
                    tokio::task::block_in_place(move || handle.block_on(future))
                }
                _ => unreachable!("unsupported runtime flavor"),
            }
        }
        Err(e) => {
            log::warn!("Error getting current runtime handle: {:?}", e);
            // Fallback behavior: Create a new runtime in a separate thread
            std::thread::spawn(move || {
                let rt = tokio::runtime::Runtime::new().expect("Failed to create runtime");
                rt.block_on(async {
                    future.await;
                });
            });
        }
    }
}

fn dropper_task_sender() -> &'static tokio::sync::mpsc::UnboundedSender<BoxFuture<'static, ()>> {
    DROP_TASK_SENDER.get_or_init(|| {
        let (dropper_tx, mut dropper_rx) = tokio::sync::mpsc::unbounded_channel();
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .thread_name("testcontainers-drop-worker")
                .enable_all()
                .build()
                .expect("failed to create dropper runtime")
                .block_on(async move {
                    while let Some(future) = dropper_rx.recv().await {
                        future.await;
                    }
                });
        });

        dropper_tx
    })
}
