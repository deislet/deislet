use super::queue_client::QueueClient;
use deno_core::{extension, op2, OpState};
use std::cell::RefCell;
use std::rc::Rc;

// In-memory queue (as fallback)
pub type MemoryQueue = Rc<RefCell<Vec<(String, String)>>>;

#[op2(async)]
#[string]
async fn op_queue_send(
    state: Rc<RefCell<OpState>>,
    #[string] queue_name: String,
    #[string] message: String,
) -> Result<String, std::io::Error> {
    let client_opt = {
        let state_borrow = state.borrow();
        state_borrow.try_borrow::<QueueClient>().cloned()
    };

    if let Some(client) = client_opt {
        return client
            .send(queue_name, message)
            .await
            .map_err(|e| std::io::Error::other(e.to_string()));
    }

    // Fallback to memory
    let state_borrow = state.borrow();
    if let Some(mem_queue) = state_borrow.try_borrow::<MemoryQueue>() {
        mem_queue.borrow_mut().push((queue_name.clone(), message));
        tracing::debug!("Queue message pushed to memory: {}", queue_name);
        return Ok("mem-id".to_string());
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "Queue service not available",
    ))
}

extension!(denix_queue_ops, ops = [op_queue_send],);

pub fn init_queue_extension() -> deno_core::Extension {
    denix_queue_ops::init()
}
