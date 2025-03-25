use crate::error;
use async_fn_traits::AsyncFn2;
use log::debug;
use std::{sync::Arc, time::Duration};
use tokio::{sync::Notify, time};
use tonic_lnd::tonic::{Code, Status};
/// Delay between retry attempts in seconds
const RETRY_DELAY: Duration = Duration::from_secs(5);
/// Maximum number of retry attempts for operations
const MAX_RETRY_ATTEMPTS: u8 = 5;

/// Helper function to determine if an error is retryable (network-related)
fn is_retryable_error(status: &Status) -> bool {
    status.code() == Code::Unavailable
        || status.code() == Code::DeadlineExceeded
        || status.code() == Code::Aborted
        || status.code() == Code::Unknown
}

pub(crate) struct Retryable<T> {
    client: T,
}

impl<T> Retryable<T> {
    pub(crate) fn new(client: T) -> Self {
        Self { client }
    }

    pub(crate) fn into_inner(self) -> T {
        self.client
    }

    pub(crate) async fn with_infinite_retries<F, R, Req>(
        &mut self,
        f: F,
        req: Req,
        retry_notify: Option<Arc<Notify>>,
    ) -> Result<R, Status>
    where
        F: for<'a> AsyncFn2<&'a mut T, Req, Output = Result<R, Status>>,
        Req: Clone,
    {
        let delay = RETRY_DELAY;
        let req_type_name = std::any::type_name::<Req>();
        debug!("Starting request {req_type_name} with infinite retries");
        loop {
            match f(&mut self.client, req.clone()).await {
                Ok(response) => {
                    return Ok(response);
                }
                Err(e) => {
                    // Only retry on network-related errors
                    if is_retryable_error(&e) {
                        error!("Error requesting {}, retrying: {}", req_type_name, e);
                        if let Some(notifier) = &retry_notify {
                            notifier.notify_waiters();
                        }
                        time::sleep(delay).await;
                        continue;
                    } else {
                        error!("Failed to request {} : {}", req_type_name, e);
                        return Err(e);
                    }
                }
            }
        }
    }

    pub(crate) async fn with_max_attempts<F, R, Req>(
        &mut self,
        f: F,
        req: Req,
        max_attempts: Option<u8>,
    ) -> Result<R, Status>
    where
        F: for<'a> AsyncFn2<&'a mut T, Req, Output = Result<R, Status>>,
        Req: Clone,
    {
        let delay = RETRY_DELAY;
        let mut attempt = 0;
        let max_retry_attempts = max_attempts.unwrap_or(MAX_RETRY_ATTEMPTS);
        let req_type_name = std::any::type_name::<Req>();

        loop {
            match f(&mut self.client, req.clone()).await {
                Ok(response) => {
                    return Ok(response);
                }
                Err(e) => {
                    // Only retry on network-related errors
                    if is_retryable_error(&e) {
                        attempt += 1;

                        if attempt >= max_retry_attempts {
                            error!(
                                "Failed to request {} with max_attempts after {} attempts: {}",
                                req_type_name, attempt, e
                            );
                            return Err(e);
                        }
                        error!(
                            "Error to request {} (attempt {}/{}): {}",
                            req_type_name, attempt, max_retry_attempts, e
                        );
                        time::sleep(delay).await;
                        continue;
                    } else {
                        error!(
                            "Failed to subscribe to events after {} attempts: {e}",
                            attempt
                        );
                        return Err(e);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;

    use super::*;
    use mockall::{automock, predicate::*};

    use tonic_lnd::tonic::{Code, Status};

    // Mock trait for the LightningClient
    #[automock]
    pub(crate) trait LightningClient {
        fn some_method(
            &mut self,
            req: String,
        ) -> impl Future<Output = Result<String, Status>> + Send;
    }

    // Helper function to create a Status with specific code
    fn create_status(code: Code, message: &str) -> Status {
        Status::new(code, message.to_string())
    }

    #[tokio::test]
    async fn test_successful_first_attempt() {
        let mut mock_client = MockLightningClient::new();
        mock_client
            .expect_some_method()
            .times(1)
            .with(eq("test_request".to_string()))
            .returning(|_| Box::pin(async { Ok("success".to_string()) }));

        let mut retryable = Retryable::new(mock_client);
        let result = retryable
            .with_infinite_retries(
                MockLightningClient::some_method,
                "test_request".to_string(),
                None,
            )
            .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "success");
    }

    #[tokio::test]
    async fn test_retry_success_after_failures() {
        let mut call_count = 0;
        let mut mock_client = MockLightningClient::new();
        mock_client
            .expect_some_method()
            .times(3)
            .with(eq("test_request".to_string()))
            .returning(move |_| {
                call_count += 1;
                match call_count {
                    1 => Box::pin(async {
                        Err(create_status(Code::Unavailable, "temp unavailable"))
                    }),
                    2 => Box::pin(async {
                        Err(create_status(Code::Unavailable, "temp unavailable"))
                    }),
                    3 => Box::pin(async { Ok("success".to_string()) }),
                    _ => panic!("Unexpected call count"),
                }
            });

        let mut retryable = Retryable::new(mock_client);
        let result = retryable
            .with_infinite_retries(
                MockLightningClient::some_method,
                "test_request".to_string(),
                None,
            )
            .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "success");
    }

    #[tokio::test]
    async fn test_non_retryable_error() {
        let mut mock_client = MockLightningClient::new();
        mock_client
            .expect_some_method()
            .times(1)
            .with(eq("test_request".to_string()))
            .returning(|_| {
                Box::pin(async { Err(create_status(Code::InvalidArgument, "temp unavailable")) })
            });

        let mut retryable = Retryable::new(mock_client);
        let result = retryable
            .with_infinite_retries(
                MockLightningClient::some_method,
                "test_request".to_string(),
                None,
            )
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument);
    }

    #[tokio::test]
    async fn test_retry_with_max_attempts_ends_with_error() {
        let mut mock_client = MockLightningClient::new();
        let mut call_count = 0;
        mock_client
            .expect_some_method()
            .times(2)
            .with(eq("test_request".to_string()))
            .returning(move |_| {
                call_count += 1;
                match call_count {
                    1 => Box::pin(async {
                        Err(create_status(Code::Unavailable, "temp unavailable"))
                    }),
                    2 => Box::pin(async {
                        Err(create_status(Code::Unavailable, "temp unavailable"))
                    }),
                    3 => Box::pin(async { Ok("success".to_string()) }),
                    _ => panic!("Unexpected call count"),
                }
            });

        let mut retryable = Retryable::new(mock_client);
        let result = retryable
            .with_max_attempts(
                MockLightningClient::some_method,
                "test_request".to_string(),
                Some(2),
            )
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), Code::Unavailable);
    }

    #[tokio::test]
    async fn test_retry_with_max_attempts_ends_with_success() {
        let mut mock_client = MockLightningClient::new();
        let mut call_count = 0;
        mock_client
            .expect_some_method()
            .times(2)
            .with(eq("test_request".to_string()))
            .returning(move |_| {
                call_count += 1;
                match call_count {
                    1 => Box::pin(async {
                        Err(create_status(Code::Unavailable, "temp unavailable"))
                    }),
                    2 => Box::pin(async { Ok("success".to_string()) }),
                    _ => panic!("Unexpected call count"),
                }
            });

        let mut retryable = Retryable::new(mock_client);
        let result = retryable
            .with_max_attempts(
                MockLightningClient::some_method,
                "test_request".to_string(),
                Some(2),
            )
            .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "success");
    }

    #[tokio::test]
    async fn test_retry_with_max_attempts_non_retryable_error() {
        let mut mock_client = MockLightningClient::new();
        let mut call_count = 0;
        mock_client
            .expect_some_method()
            .times(1)
            .with(eq("test_request".to_string()))
            .returning(move |_| {
                call_count += 1;
                match call_count {
                    1 => Box::pin(async {
                        Err(create_status(Code::InvalidArgument, "invalid argument"))
                    }),
                    _ => panic!("Unexpected call count"),
                }
            });

        let mut retryable = Retryable::new(mock_client);
        let result = retryable
            .with_max_attempts(
                MockLightningClient::some_method,
                "test_request".to_string(),
                Some(2),
            )
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument);
    }
}
