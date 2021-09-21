use crate::configuration::Configuration;
use crate::http_server_factory::{HttpServerFactory, HttpServerHandle};
use crate::FederatedServerError;
use apollo_router_core::{FetchError, GraphQLFetcher, GraphQLRequest};
use bytes::Bytes;
use futures::channel::oneshot;
use futures::prelude::*;
use parking_lot::RwLock;
use std::sync::Arc;
use warp::hyper::Response;
use warp::{
    http::{header::HeaderValue, StatusCode, Uri},
    hyper::Body,
    Filter,
};
use warp::{Rejection, Reply};

/// A basic http server using warp.
/// Uses streaming as primary method of response.
/// Redirects to studio for GET requests.
#[derive(Debug)]
pub(crate) struct WarpHttpServerFactory;

impl WarpHttpServerFactory {
    pub(crate) fn new() -> Self {
        Self
    }
}

impl HttpServerFactory for WarpHttpServerFactory {
    fn create<F>(
        &self,
        graph: Arc<RwLock<F>>,
        configuration: Arc<RwLock<Configuration>>,
    ) -> HttpServerHandle
    where
        F: GraphQLFetcher + 'static,
    {
        let (shutdown_sender, shutdown_receiver) = oneshot::channel();
        let listen_address = configuration.read().server.listen;

        let cors = configuration
            .read()
            .server
            .cors
            .as_ref()
            .map(|cors_configuration| cors_configuration.into_warp_middleware())
            .unwrap_or_else(warp::cors);

        let routes = redirect_to_studio()
            .or(perform_graphql_request(graph, configuration))
            .with(cors);

        let (actual_listen_address, server) =
            warp::serve(routes).bind_with_graceful_shutdown(listen_address, async {
                shutdown_receiver.await.ok();
            });

        // Spawn the server into a runtime
        let server_future = tokio::task::spawn(server)
            .map_err(|_| FederatedServerError::HttpServerLifecycleError)
            .boxed();

        HttpServerHandle {
            shutdown_sender,
            server_future,
            listen_address: actual_listen_address,
        }
    }
}

fn redirect_to_studio() -> impl Filter<Extract = (Box<dyn Reply>,), Error = Rejection> + Clone {
    warp::get()
        .and(warp::path::end().or(warp::path("graphql")))
        .and(warp::header::value("Host"))
        .map(|_, host: HeaderValue| {
            host.to_str()
                .map(|h| -> Box<dyn Reply> {
                    Box::new(warp::redirect::temporary(
                        format!(
                            "https://studio.apollographql.com/sandbox?endpoint=http://{}",
                            h
                        )
                        .parse::<Uri>()
                        .unwrap(),
                    ))
                })
                .unwrap_or_else(|_| {
                    Box::new(warp::reply::with_status(
                        "Invalid request Host header",
                        StatusCode::BAD_REQUEST,
                    ))
                })
        })
        .boxed()
}

fn perform_graphql_request<F>(
    graph: Arc<RwLock<F>>,
    configuration: Arc<RwLock<Configuration>>,
) -> impl Filter<Extract = (Response<Body>,), Error = Rejection> + Clone
where
    F: GraphQLFetcher + 'static,
{
    warp::post()
        .and(warp::path::end().or(warp::path("graphql")))
        .and(warp::body::json())
        .map(move |_, graphql_request: GraphQLRequest| {
            let default_tracing_dispatcher = {
                let lock = configuration.read();
                lock.subscriber
                    .clone()
                    .map(tracing::Dispatch::new)
                    .unwrap_or_default()
            };
            let stream = tracing::dispatcher::with_default(&default_tracing_dispatcher, || {
                graph.read().stream(graphql_request)
            });
            Response::new(Body::wrap_stream(
                stream
                    .enumerate()
                    .map(|(index, res)| match serde_json::to_string(&res) {
                        Ok(bytes) => Ok(Bytes::from(bytes)),
                        Err(err) => {
                            // We didn't manage to serialise the response!
                            // Do our best to send some sort of error back.
                            serde_json::to_string(
                                &FetchError::MalformedResponse {
                                    reason: err.to_string(),
                                }
                                .to_response(index == 0),
                            )
                            .map(Bytes::from)
                        }
                    })
                    .boxed(),
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::configuration::Cors;
    use apollo_router_core::{
        FetchError, GraphQLFetcher, GraphQLRequest, GraphQLResponse, GraphQLResponseStream,
    };
    use mockall::{mock, predicate::*};
    use reqwest::header::{
        ACCEPT, ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_METHODS,
        ACCESS_CONTROL_ALLOW_ORIGIN, ACCESS_CONTROL_REQUEST_HEADERS, ACCESS_CONTROL_REQUEST_METHOD,
        LOCATION, ORIGIN,
    };
    use reqwest::redirect::Policy;
    use reqwest::{Client, Method, StatusCode};
    use serde_json::json;
    use std::net::SocketAddr;
    use std::str::FromStr;

    macro_rules! assert_header {
        ($response:expr, $header:expr, $expected:expr $(, $msg:expr)?) => {
            assert_eq!(
                $response
                    .headers()
                    .get_all($header)
                    .iter()
                    .map(|v|v.to_str().unwrap().to_string())
                    .collect::<Vec<_>>(),
                $expected
                $(, $msg)*
            );
        };
    }

    /// `assert_header_contains` works like `assert_headers`,
    /// except it doesn't care for the order of the items
    macro_rules! assert_header_contains {
        ($response:expr, $header:expr, $expected:expr $(, $msg:expr)?) => {
            let header_values = $response
            .headers()
            .get_all($header)
            .iter()
            .map(|v|v.to_str().unwrap().to_string())
            .collect::<Vec<_>>();

            for e in $expected {
                assert!(
                    header_values
                        .iter()
                        .find(|header_value| header_value.contains(&e.to_string()))
                        .is_some(),
                    $($msg)*
                );
            }

        };
    }

    mock! {
        #[derive(Debug)]
        MyGraphQLFetcher{}
        impl GraphQLFetcher for MyGraphQLFetcher {   // specification of the trait to mock
            fn stream(&self, request: GraphQLRequest) -> GraphQLResponseStream;
        }
    }

    fn init(listen_address: &str) -> (Arc<RwLock<MockMyGraphQLFetcher>>, HttpServerHandle, Client) {
        let _ = env_logger::builder().is_test(true).try_init();
        let fetcher = MockMyGraphQLFetcher::new();
        let server_factory = WarpHttpServerFactory::new();
        let fetcher = Arc::new(RwLock::new(fetcher));
        let server = server_factory.create(
            fetcher.to_owned(),
            Arc::new(RwLock::new(
                Configuration::builder()
                    .server(
                        crate::configuration::Server::builder()
                            .listen(SocketAddr::from_str(listen_address).unwrap())
                            .cors(Some(
                                Cors::builder()
                                    .origins(vec!["http://studio".to_string()])
                                    .build(),
                            ))
                            .build(),
                    )
                    .subgraphs(Default::default())
                    .build(),
            )),
        );
        let client = reqwest::Client::builder()
            .redirect(Policy::none())
            .build()
            .unwrap();
        (fetcher, server, client)
    }

    #[tokio::test]
    async fn redirect_to_studio() -> Result<(), FederatedServerError> {
        let (_fetcher, server, client) = init("127.0.0.1:0");

        for url in vec![
            format!("http://{}/", server.listen_address),
            format!("http://{}/graphql", server.listen_address),
        ] {
            let response = client
                .get(url)
                .header(ACCEPT, "text/html")
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
            assert_header!(
                &response,
                LOCATION,
                vec![format!(
                    "https://studio.apollographql.com/sandbox?endpoint=http://{}",
                    server.listen_address
                )
                .to_string()],
                "Incorrect redirect url"
            );
        }

        server.shutdown().await
    }

    #[tokio::test]
    async fn malformed_request() -> Result<(), FederatedServerError> {
        let (_fetcher, server, client) = init("127.0.0.1:0");

        let response = client
            .post(format!("http://{}/graphql", server.listen_address))
            .body("Garbage")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        server.shutdown().await
    }

    #[tokio::test]
    async fn response() -> Result<(), FederatedServerError> {
        let expected_response = GraphQLResponse::builder()
            .data(json!({"response": "yay"}))
            .build();
        let example_response = expected_response.clone();
        let (fetcher, server, client) = init("127.0.0.1:0");
        {
            fetcher
                .write()
                .expect_stream()
                .times(1)
                .return_once(move |_| futures::stream::iter(vec![example_response]).boxed());
        }
        let response = client
            .post(format!("http://{}/graphql", server.listen_address))
            .body(
                json!(
                {
                  "query": "query",
                })
                .to_string(),
            )
            .send()
            .await
            .unwrap()
            .error_for_status()
            .expect("unexpected response");

        assert_eq!(
            response.json::<GraphQLResponse>().await.unwrap(),
            expected_response,
        );

        server.shutdown().await
    }

    #[tokio::test]
    async fn response_failure() -> Result<(), FederatedServerError> {
        let (fetcher, server, client) = init("127.0.0.1:0");
        {
            fetcher.write().expect_stream().times(1).return_once(|_| {
                futures::stream::iter(vec![FetchError::SubrequestHttpError {
                    service: "Mock service".to_string(),
                    reason: "Mock error".to_string(),
                }
                .to_response(true)])
                .boxed()
            });
        }
        let response = client
            .post(format!("http://{}/graphql", server.listen_address))
            .body(
                json!(
                {
                  "query": "query",
                })
                .to_string(),
            )
            .send()
            .await
            .ok()
            .unwrap()
            .json::<GraphQLResponse>()
            .await
            .unwrap();

        assert_eq!(
            response,
            FetchError::SubrequestHttpError {
                service: "Mock service".to_string(),
                reason: "Mock error".to_string(),
            }
            .to_response(true)
        );
        server.shutdown().await
    }

    #[tokio::test]
    async fn cors_preflight() -> Result<(), FederatedServerError> {
        let (_fetcher, server, client) = init("127.0.0.1:0");

        for url in vec![
            format!("http://{}/", server.listen_address),
            format!("http://{}/graphql", server.listen_address),
        ] {
            let response = client
                .request(Method::OPTIONS, &url)
                .header(ACCEPT, "text/html")
                .header(ORIGIN, "http://studio")
                .header(ACCESS_CONTROL_REQUEST_METHOD, "POST")
                .header(ACCESS_CONTROL_REQUEST_HEADERS, "Content-type")
                .send()
                .await
                .unwrap();

            assert_header!(
                &response,
                ACCESS_CONTROL_ALLOW_ORIGIN,
                vec!["http://studio"],
                "Incorrect access control allow origin header"
            );
            assert_header_contains!(
                &response,
                ACCESS_CONTROL_ALLOW_HEADERS,
                vec!["content-type"],
                "Incorrect access control allow header header"
            );
            assert_header_contains!(
                &response,
                ACCESS_CONTROL_ALLOW_METHODS,
                vec!["GET", "POST", "OPTIONS"],
                "Incorrect access control allow methods header"
            );

            assert_eq!(response.status(), StatusCode::OK);
        }

        server.shutdown().await
    }
}