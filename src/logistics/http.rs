//! HTTP clients for outbound calls (Anthropic, SMS and email providers).
//!
//! `reqwest::Client::new()` has no timeout at all, so a provider that accepts a
//! connection and never answers would hang its caller forever. Every outbound
//! call goes through [`client`] instead.

use std::time::Duration;

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Long enough for a full assistant answer; a hung provider still gives up.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// A client with the standard timeouts.
pub fn client() -> reqwest::Client {
    client_with(CONNECT_TIMEOUT, REQUEST_TIMEOUT)
}

fn client_with(connect: Duration, total: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(connect)
        .timeout(total)
        .build()
        .expect("HTTP client configuration is static and valid")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    use tokio::net::TcpListener;

    #[actix_web::test]
    async fn a_server_that_never_answers_times_out_instead_of_hanging() {
        // Accepts the connection, then says nothing.
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let _server = tokio::spawn(async move {
            let (_socket, _) = listener.accept().await.expect("accept");
            tokio::time::sleep(Duration::from_secs(30)).await;
        });

        let started = Instant::now();
        let err = client_with(Duration::from_secs(1), Duration::from_millis(200))
            .get(format!("http://{addr}/"))
            .send()
            .await
            .expect_err("a silent server must fail the request");

        assert!(err.is_timeout(), "{err}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn the_standard_client_builds() {
        let _ = client();
        assert!(CONNECT_TIMEOUT < REQUEST_TIMEOUT);
    }
}
