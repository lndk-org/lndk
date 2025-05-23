use std::fmt::Debug;
use std::future::Future;
use std::ops::FnMut;
use tokio::time::{sleep, timeout, Duration};
use tonic_lnd::tonic::{Response, Status};

// If a grpc call returns an error, retry_async will retry the grpc function call in case lnd is
// still in the process of starting up. A note on implementation: We can't pass in a future directly
// to a function because futures cannot be cloned/copied in order to retry the future. Instead
// retry_async takes in an async closure that is able to "copy" the function for us so we can call
// it multiple times for retries.
pub(crate) async fn retry_async<F, Fut, D>(mut f: F, func_name: String) -> Result<D, ()>
where
    F: FnMut() -> Fut + std::marker::Copy,
    Fut: Future<Output = Result<Response<D>, Status>>,
    D: Debug,
{
    let mut retry_num = 0;
    let resp = Err(());
    while retry_num < 3 {
        sleep(Duration::from_secs(3)).await;
        match timeout(Duration::from_secs(60), f()).await {
            Ok(call_result) => match call_result {
                Err(e) => {
                    println!("retry_async: error from {}: {:?}", func_name.clone(), e);
                }
                Ok(resp) => {
                    return Ok(resp.into_inner());
                }
            },
            Err(_) => {
                println!(
                    "retry_async: timeout after 60 seconds for {} call",
                    func_name.clone()
                );
            }
        }

        retry_num += 1;
        if retry_num == 5 {
            println!("retry_async: {} call failed after 5 retries", func_name);
            panic!("{} call failed after 5 retries", func_name);
        }
    }
    resp
}
