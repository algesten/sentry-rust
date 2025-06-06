use std::time::Duration;

use ureq::http::Response;
#[cfg(any(feature = "rustls", feature = "native-tls"))]
use ureq::tls::{TlsConfig, TlsProvider};
use ureq::{Agent, Proxy};

use super::thread::TransportThread;

use crate::{sentry_debug, types::Scheme, ClientOptions, Envelope, Transport};

/// A [`Transport`] that sends events via the [`ureq`] library.
///
/// This is enabled by the `ureq` feature flag.
#[cfg_attr(doc_cfg, doc(cfg(feature = "ureq")))]
pub struct UreqHttpTransport {
    thread: TransportThread,
}

impl UreqHttpTransport {
    /// Creates a new Transport.
    pub fn new(options: &ClientOptions) -> Self {
        Self::new_internal(options, None)
    }

    /// Creates a new Transport that uses the specified [`ureq::Agent`].
    pub fn with_agent(options: &ClientOptions, agent: Agent) -> Self {
        Self::new_internal(options, Some(agent))
    }

    fn new_internal(options: &ClientOptions, agent: Option<Agent>) -> Self {
        let dsn = options.dsn.as_ref().unwrap();
        let scheme = dsn.scheme();
        let agent = agent.unwrap_or_else(|| {
            let mut builder = Agent::config_builder();

            #[cfg(feature = "native-tls")]
            {
                builder = builder.tls_config(
                    TlsConfig::builder()
                        .provider(TlsProvider::NativeTls)
                        .disable_verification(options.accept_invalid_certs)
                        .build(),
                );
            }
            #[cfg(feature = "rustls")]
            {
                builder = builder.tls_config(
                    TlsConfig::builder()
                        .provider(TlsProvider::Rustls)
                        .disable_verification(options.accept_invalid_certs)
                        .build(),
                );
            }

            let mut maybe_proxy = None;

            match (scheme, &options.http_proxy, &options.https_proxy) {
                (Scheme::Https, _, Some(proxy)) => match Proxy::new(proxy) {
                    Ok(proxy) => {
                        maybe_proxy = Some(proxy);
                    }
                    Err(err) => {
                        sentry_debug!("invalid proxy: {:?}", err);
                    }
                },
                (_, Some(proxy), _) => match Proxy::new(proxy) {
                    Ok(proxy) => {
                        maybe_proxy = Some(proxy);
                    }
                    Err(err) => {
                        sentry_debug!("invalid proxy: {:?}", err);
                    }
                },
                _ => {}
            }

            builder = builder.proxy(maybe_proxy);

            builder.build().new_agent()
        });
        let user_agent = options.user_agent.clone();
        let auth = dsn.to_auth(Some(&user_agent)).to_string();
        let url = dsn.envelope_api_url().to_string();

        let thread = TransportThread::new(move |envelope, rl| {
            let mut body = Vec::new();
            envelope.to_writer(&mut body).unwrap();
            let request = agent.post(&url).header("X-Sentry-Auth", &auth).send(&body);

            match request {
                Ok(mut response) => {
                    fn header_str<'a, B>(response: &'a Response<B>, key: &str) -> Option<&'a str> {
                        response.headers().get(key)?.to_str().ok()
                    }

                    if let Some(sentry_header) = header_str(&response, "x-sentry-rate-limits") {
                        rl.update_from_sentry_header(sentry_header);
                    } else if let Some(retry_after) = header_str(&response, "retry-after") {
                        rl.update_from_retry_after(retry_after);
                    } else if response.status() == 429 {
                        rl.update_from_429();
                    }

                    match response.body_mut().read_to_string() {
                        Err(err) => {
                            sentry_debug!("Failed to read sentry response: {}", err);
                        }
                        Ok(text) => {
                            sentry_debug!("Get response: `{}`", text);
                        }
                    }
                }
                Err(err) => {
                    sentry_debug!("Failed to send envelope: {}", err);
                }
            }
        });
        Self { thread }
    }
}

impl Transport for UreqHttpTransport {
    fn send_envelope(&self, envelope: Envelope) {
        self.thread.send(envelope)
    }
    fn flush(&self, timeout: Duration) -> bool {
        self.thread.flush(timeout)
    }

    fn shutdown(&self, timeout: Duration) -> bool {
        self.flush(timeout)
    }
}
