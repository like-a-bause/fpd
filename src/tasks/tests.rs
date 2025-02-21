use super::service::{ProxyDataSource, ProxyService, WasmModules};
use fiberplane::base64uuid::Base64Uuid;
use fiberplane::models::providers::{Error, HttpRequestError, TIMESERIES_QUERY_TYPE};
use fiberplane::models::{data_sources::DataSourceStatus, names::Name, proxies::*};
use fiberplane::provider_bindings::Blob;
use fiberplane::provider_runtime::spec::types::ProviderRequest;
use futures::{select, FutureExt, SinkExt, StreamExt};
use http::{Request, Response, StatusCode};
use httpmock::prelude::*;
use hyper::header::HeaderValue;
use once_cell::sync::Lazy;
use serde_json::{json, Map, Value};
use std::iter::FromIterator;
use std::{collections::HashMap, path::Path, time::Duration};
use test_log::test;
use tokio::{join, net::TcpListener, sync::broadcast};
use tokio_tungstenite::{accept_hdr_async, tungstenite::Message};

static TOKEN: Lazy<ProxyToken> = Lazy::new(|| {
    ProxyToken::builder()
        .workspace_id(Base64Uuid::new())
        .proxy_name(Name::from_static("test-proxy"))
        .token("MVPpfxAYRxcQ4rFZUB7RRzirzwhR7htlkU3zcDm-pZk")
        .build()
});

async fn mock_prometheus() -> (MockServer, Vec<ProxyDataSource>) {
    let prometheus = MockServer::start_async().await;

    let data_sources = vec![ProxyDataSource {
        name: Name::from_static("prometheus-dev"),
        description: None,
        provider_type: "prometheus".to_string(),
        config: Map::from_iter(vec![("url".to_string(), Value::String(prometheus.url("")))]),
    }];

    (prometheus, data_sources)
}

#[test]
fn parses_data_sources_from_yaml() {
    let yaml = "
- name: prometheus-production
  providerType: prometheus
  description: Prometheus on production cluster
  config:
    url: http://localhost:9090
- name: elasticsearch-dev
  providerType: elasticsearch
  config:
    url: http://localhost:9200";
    let data_sources: Vec<ProxyDataSource> = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(data_sources.len(), 2);
    assert_eq!(
        data_sources[0].name,
        Name::from_static("prometheus-production")
    );
    assert_eq!(data_sources[0].provider_type, "prometheus");
    assert_eq!(
        data_sources[0].description,
        Some("Prometheus on production cluster".to_string())
    );
    assert_eq!(
        data_sources[0].config,
        *json!({
            "url": "http://localhost:9090"
        })
        .as_object()
        .unwrap()
    );
    assert_eq!(data_sources[1].name, Name::from_static("elasticsearch-dev"));
    assert_eq!(data_sources[1].provider_type, "elasticsearch");
    assert_eq!(data_sources[1].description, None);
    assert_eq!(
        data_sources[1].config,
        *json!({
            "url": "http://localhost:9200"
        })
        .as_object()
        .unwrap()
    );
}

#[test(tokio::test)]
async fn sends_auth_token_header() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let service = ProxyService::new(
        format!("http://{addr}").parse().unwrap(),
        TOKEN.clone(),
        Default::default(),
        Default::default(),
        5,
        None,
        Duration::from_secs(300),
    );

    let handle_connection = async move {
        let (stream, _) = listener.accept().await.unwrap();
        accept_hdr_async(stream, |req: &Request<()>, mut res: Response<()>| {
            assert_eq!(req.headers().get("fp-auth-token").unwrap(), &TOKEN.token);
            assert_eq!(
                req.uri().path(),
                format!(
                    "/api/workspaces/{}/proxies/{}/ws",
                    TOKEN.workspace_id, TOKEN.proxy_name
                )
            );
            res.headers_mut()
                .insert("fp-conn-id", HeaderValue::from_static("conn-id"));
            Ok(res)
        })
        .await
        .unwrap();
    };

    let (tx, _) = broadcast::channel(3);
    select! {
      result = service.connect(tx).fuse() => result.unwrap(),
      _ = handle_connection.fuse() => {}
    }
}

#[test(tokio::test)]
async fn sends_data_sources_on_connect() {
    let connected_prometheus = MockServer::start();
    let connected_prometheus_mock = connected_prometheus.mock(|when, then| {
        when.method("GET").path("/api/v1/query");
        then.status(200).body("{}");
    });
    let connected_elasticsearch = MockServer::start();
    let connected_elasticsearch_mock = connected_elasticsearch.mock(|when, then| {
        when.method("GET").path("/_xpack");
        then.status(200).body("{}");
    });
    let disconnected_prometheus = MockServer::start();
    let disconnected_prometheus_mock = disconnected_prometheus.mock(|when, then| {
        when.method("GET").path("/api/v1/query");
        then.status(500).body("Some Error");
    });

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let data_sources = vec![
        ProxyDataSource {
            name: Name::from_static("connected-prometheus"),
            provider_type: "prometheus".to_string(),
            description: None,
            config: Map::from_iter([("url".into(), json!(connected_prometheus.url("")))]),
        },
        ProxyDataSource {
            name: Name::from_static("connected-elasticsearch"),
            provider_type: "elasticsearch".to_string(),
            description: None,
            config: Map::from_iter([("url".into(), json!(connected_elasticsearch.url("")))]),
        },
        ProxyDataSource {
            name: Name::from_static("disconnected-prometheus"),
            provider_type: "prometheus".to_string(),
            description: None,
            config: Map::from_iter([("url".into(), json!(disconnected_prometheus.url("")))]),
        },
        // We don't have the proxy provider wasm module so this tests
        // what happens if you specify a provider that we don't have
        ProxyDataSource {
            name: Name::from_static("unknown-data-source"),
            provider_type: "other-provider".to_string(),
            description: None,
            config: Map::from_iter([("url".into(), json!("http://localhost:1234"))]),
        },
    ];
    let service = ProxyService::init(
        format!("ws://{addr}").parse().unwrap(),
        TOKEN.clone(),
        Path::new("./providers"),
        data_sources,
        5,
        None,
        Duration::from_secs(300),
    )
    .await;

    let handle_connection = async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_hdr_async(stream, |_req: &Request<()>, mut res: Response<()>| {
            res.headers_mut()
                .insert("fp-conn-id", HeaderValue::from_static("conn-id"));
            Ok(res)
        })
        .await
        .unwrap();

        let message = ws.next().await.unwrap().unwrap();
        let message = match message {
            Message::Binary(message) => ProxyMessage::deserialize_msgpack(message).unwrap(),
            _ => panic!("wrong type"),
        };
        let data_sources = if let ProxyMessagePayload::SetDataSources(SetDataSourcesMessage {
            data_sources,
            ..
        }) = message.payload
        {
            data_sources
        } else {
            panic!("wrong type");
        };

        assert_eq!(data_sources.len(), 4);
        let data_sources: HashMap<_, _> = data_sources
            .into_iter()
            .map(|data_source| (data_source.name.clone(), data_source))
            .collect();

        let connected_prometheus = data_sources
            .get(&Name::from_static("connected-prometheus"))
            .unwrap();
        assert_eq!(connected_prometheus.provider_type, "prometheus");
        assert_eq!(connected_prometheus.description, None);
        assert_eq!(connected_prometheus.status, DataSourceStatus::Connected);

        let connected_elasticsearch = data_sources
            .get(&Name::from_static("connected-elasticsearch"))
            .unwrap();
        assert_eq!(connected_elasticsearch.provider_type, "elasticsearch");
        assert_eq!(connected_elasticsearch.description, None);
        assert_eq!(connected_elasticsearch.status, DataSourceStatus::Connected);

        let disconnected_prometheus = data_sources
            .get(&Name::from_static("disconnected-prometheus"))
            .unwrap();
        assert_eq!(disconnected_prometheus.provider_type, "prometheus");
        assert_eq!(disconnected_prometheus.description, None);
        assert_eq!(
            disconnected_prometheus.status,
            DataSourceStatus::Error(Error::Http {
                error: HttpRequestError::ServerError {
                    status_code: 500,
                    response: b"Some Error".to_vec().into()
                }
            })
        );

        let unknown_data_source = data_sources
            .get(&Name::from_static("unknown-data-source"))
            .unwrap();
        assert_eq!(unknown_data_source.provider_type, "other-provider");
        assert_eq!(unknown_data_source.description, None);
        assert!(matches!(
            unknown_data_source.status,
            DataSourceStatus::Error(Error::Invocation { .. })
        ));
    };

    let (tx, _) = broadcast::channel(3);
    select! {
      result = service.connect(tx).fuse() => result.unwrap(),
      _ = handle_connection.fuse() => {}
    }
    connected_prometheus_mock.assert();
    connected_elasticsearch_mock.assert();
    disconnected_prometheus_mock.assert();
}

#[test(tokio::test)]
async fn checks_data_source_status_on_interval() {
    let (mock_server, data_sources) = mock_prometheus().await;
    let mut connected_prometheus_mock = mock_server.mock(|when, then| {
        when.method("GET").path("/api/v1/query");
        then.status(200).body("{}");
    });
    let disconnected_prometheus_mock = mock_server.mock(|when, then| {
        when.method("GET").path("/api/v1/query");
        then.status(500).body("Internal Server Error");
    });

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let service = ProxyService::init(
        format!("ws://{addr}").parse().unwrap(),
        TOKEN.clone(),
        Path::new("./providers"),
        data_sources,
        5,
        None,
        Duration::from_millis(200),
    )
    .await;

    let handle_connection = async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_hdr_async(stream, |_req: &Request<()>, mut res: Response<()>| {
            res.headers_mut()
                .insert("fp-conn-id", HeaderValue::from_static("conn-id"));
            Ok(res)
        })
        .await
        .unwrap();
        for _i in 0..2 {
            let message = ws.next().await.unwrap().unwrap();
            let message = match message {
                Message::Binary(message) => ProxyMessage::deserialize_msgpack(message).unwrap(),
                _ => panic!("wrong type"),
            };
            if let ProxyMessagePayload::SetDataSources(SetDataSourcesMessage {
                mut data_sources,
                ..
            }) = message.payload
            {
                assert_eq!(
                    data_sources.pop().unwrap().status,
                    DataSourceStatus::Connected
                );
            } else {
                panic!();
            };
        }
        connected_prometheus_mock.assert_hits(2);
        connected_prometheus_mock.delete();

        let message = ws.next().await.unwrap().unwrap();
        let message = match message {
            Message::Binary(message) => ProxyMessage::deserialize_msgpack(message).unwrap(),
            _ => panic!("wrong type"),
        };
        if let ProxyMessagePayload::SetDataSources(SetDataSourcesMessage {
            mut data_sources, ..
        }) = message.payload
        {
            assert_eq!(
                data_sources.pop().unwrap().status,
                DataSourceStatus::Error(Error::Http {
                    error: HttpRequestError::ServerError {
                        status_code: 500,
                        response: b"Internal Server Error".to_vec().into()
                    }
                })
            );
        } else {
            panic!();
        };
    };

    let (tx, _) = broadcast::channel(3);
    select! {
      result = service.connect(tx).fuse() => result.unwrap(),
      _ = handle_connection.fuse() => {}
    }

    disconnected_prometheus_mock.assert();
}

#[test(tokio::test)]
async fn sends_pings() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let service = ProxyService::new(
        format!("ws://{addr}").parse().unwrap(),
        TOKEN.clone(),
        WasmModules::new(),
        Default::default(),
        5,
        None,
        Duration::from_secs(300),
    );

    tokio::time::pause();

    let handle_connection = async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_hdr_async(stream, |_req: &Request<()>, mut res: Response<()>| {
            res.headers_mut()
                .insert("fp-conn-id", HeaderValue::from_static("conn-id"));
            Ok(res)
        })
        .await
        .unwrap();
        // first message is data sources
        ws.next().await.unwrap().unwrap();

        tokio::time::advance(tokio::time::Duration::from_secs(45)).await;
        tokio::time::resume();

        // Next should be ping
        let message = ws.next().await.unwrap().unwrap();
        match message {
            Message::Ping(_) => {}
            _ => panic!("expected ping"),
        };
    };

    let (tx, _) = broadcast::channel(3);
    select! {
      result = service.connect(tx).fuse() => result.unwrap(),
      _ = handle_connection.fuse() => {}
    }
}

#[test(tokio::test)]
async fn health_check_endpoints() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let service_addr = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap();

    let service = ProxyService::new(
        format!("ws://{addr}").parse().unwrap(),
        TOKEN.clone(),
        HashMap::new(),
        HashMap::new(),
        5,
        Some(service_addr),
        Duration::from_secs(300),
    );

    let handle_connection = async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_hdr_async(stream, |_req: &Request<()>, mut res: Response<()>| {
            res.headers_mut()
                .insert("fp-conn-id", HeaderValue::from_static("conn-id"));
            Ok(res)
        })
        .await
        .unwrap();
        ws.next().await.unwrap().unwrap();

        let check_endpoint = |path: &'static str| async move {
            reqwest::get(format!("http://{service_addr}{path}"))
                .await
                .unwrap()
                .status()
        };

        // Check status while connected
        assert_eq!(StatusCode::OK, check_endpoint("").await);
        assert_eq!(StatusCode::OK, check_endpoint("/health").await);

        // Check status after disconnect
        drop(ws);
        assert_eq!(StatusCode::OK, check_endpoint("").await);
        assert_eq!(StatusCode::BAD_GATEWAY, check_endpoint("/health").await);
    };

    let connect = async move {
        let (tx, _) = broadcast::channel(3);
        service.connect(tx).await.unwrap();
    };

    select! {
      _ = connect.fuse() => {}
      _ = handle_connection.fuse() => {}
    }
}

#[test(tokio::test)]
async fn returns_error_for_query_to_unknown_provider() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let service = ProxyService::new(
        format!("ws://{addr}").parse().unwrap(),
        TOKEN.clone(),
        WasmModules::new(),
        Default::default(),
        5,
        None,
        Duration::from_secs(300),
    );

    let handle_connection = async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_hdr_async(stream, |_req: &Request<()>, mut res: Response<()>| {
            res.headers_mut()
                .insert("fp-conn-id", HeaderValue::from_static("conn-id"));
            Ok(res)
        })
        .await
        .unwrap();
        // first message is data sources
        ws.next().await.unwrap().unwrap();

        let op_id = Base64Uuid::new();
        let message = ServerMessage::new_invoke_proxy_request(
            b"fake payload".to_vec(),
            Name::from_static("data-source-1"),
            1,
            op_id,
        );
        let message = message.serialize_msgpack();
        ws.send(Message::Binary(message)).await.unwrap();

        // Parse the query result
        let response = ws.next().await.unwrap().unwrap();
        let response = match response {
            Message::Binary(message) => ProxyMessage::deserialize_msgpack(message).unwrap(),
            _ => panic!("wrong message type"),
        };
        let error = match response.payload {
            ProxyMessagePayload::Error(error) => error,
            other => panic!("wrong message type {:?}", other),
        };
        assert_eq!(response.op_id.unwrap(), op_id);
        assert!(matches!(error.error, Error::NotFound));
    };

    let (tx, _) = broadcast::channel(3);
    select! {
      result = service.connect(tx).fuse() => result.unwrap(),
      _ = handle_connection.fuse() => {}
    }
}

#[test(tokio::test)]
async fn calls_provider_with_query_and_sends_result() {
    let (prometheus, data_sources) = mock_prometheus().await;
    let status_response = r#"{
       "status" : "success",
       "data" : {
          "resultType" : "vector",
          "result" : [
             {
                "metric" : {
                   "__name__" : "up",
                   "job" : "prometheus",
                   "instance" : "localhost:9090"
                },
                "value": [ 1435781451.781, "1" ]
             }
          ]
       }
    }"#;
    let status_mock = prometheus.mock(|when, then| {
        when.path("/api/v1/query");
        then.status(200).body(status_response);
    });

    let query_response = r#"{
        "status" : "success",
        "data" : {
           "resultType" : "matrix",
           "result" : [
              {
                 "metric" : {
                    "__name__" : "up",
                    "job" : "prometheus",
                    "instance" : "localhost:9090"
                 },
                 "values": [[ 1435781451.781, "1" ]]
              }
           ]
        }
     }"#;

    let query_mock = prometheus.mock(|when, then| {
        when.path("/api/v1/query_range");
        then.status(200).body(query_response);
    });

    // Create a websocket listener for the proxy to connect to
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let service = ProxyService::init(
        format!("ws://{addr}").parse().unwrap(),
        TOKEN.clone(),
        Path::new("./providers"),
        data_sources,
        5,
        None,
        Duration::from_secs(300),
    )
    .await;

    // After the proxy connects, send it a query
    let handle_connection = async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_hdr_async(stream, |_req: &Request<()>, mut res: Response<()>| {
            res.headers_mut()
                .insert("fp-conn-id", HeaderValue::from_static("conn-id"));
            Ok(res)
        })
        .await
        .unwrap();
        ws.next().await.unwrap().unwrap();

        // Send query
        let op_id = Base64Uuid::new();
        let request = ProviderRequest::builder()
            .query_type(TIMESERIES_QUERY_TYPE)
            .query_data(Blob::builder()
                            .data(b"query=test%20query&time_range=2022-08-31T11:00:00.000Z+2022-08-31T12:00:00.000Z".to_vec())
                            .mime_type("application/x-www-form-urlencoded")
                .build())
            .config(Value::Null)
            .build();
        let message = ServerMessage::new_invoke_proxy_request(
            rmp_serde::to_vec(&request).unwrap(),
            Name::from_static("prometheus-dev"),
            2,
            op_id,
        );
        let message = message.serialize_msgpack();
        ws.send(Message::Binary(message)).await.unwrap();

        // Parse the query result
        let response = ws.next().await.unwrap().unwrap();
        let response = match response {
            Message::Binary(message) => ProxyMessage::deserialize_msgpack(message).unwrap(),
            _ => panic!("wrong message type"),
        };
        let result = match response.payload {
            ProxyMessagePayload::InvokeProxyResponse(message) => message,
            other => panic!("wrong message type: {:?}", other),
        };
        assert_eq!(response.op_id.unwrap(), op_id);
        let result: Result<Blob, Error> = rmp_serde::from_slice(&result.data).unwrap();
        assert!(result.is_ok());
    };

    let (tx, _) = broadcast::channel(3);
    select! {
      result = service.connect(tx).fuse() => result.unwrap(),
      _ = handle_connection.fuse() => {}
    }

    status_mock.assert_hits(1);
    query_mock.assert_hits(1);
}

#[test(tokio::test)]
async fn handles_multiple_concurrent_messages() {
    let (prometheus, data_sources) = mock_prometheus().await;
    // Delay the first response so the second response definitely comes back first
    let prometheus_response = r#"{
       "status" : "success",
       "data" : {
          "resultType" : "vector",
          "result" : []
       }
    }"#;
    prometheus.mock(|when, then| {
        when.path("/api/v1/query");
        then.status(200).body(prometheus_response);
    });

    let query_response = r#"{
        "status" : "success",
        "data" : {
           "resultType" : "matrix",
           "result" : [
              {
                 "metric" : {
                    "__name__" : "up",
                    "job" : "prometheus",
                    "instance" : "localhost:9090"
                 },
                 "values": [[ 1435781451.781, "1" ]]
              }
           ]
        }
     }"#;

    prometheus.mock(|when, then| {
        when.path("/api/v1/query_range").body_contains("query1");
        then.delay(Duration::from_secs(2))
            .status(200)
            .body(query_response);
    });
    prometheus.mock(|when, then| {
        when.path("/api/v1/query_range").body_contains("query2");
        then.delay(Duration::from_secs(1))
            .status(200)
            .body(query_response);
    });

    // Create a websocket listener for the proxy to connect to
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let service = ProxyService::init(
        format!("ws://{addr}").parse().unwrap(),
        TOKEN.clone(),
        Path::new("./providers"),
        data_sources,
        5,
        None,
        Duration::from_secs(300),
    )
    .await;

    // After the proxy connects, send it a query
    let handle_connection = async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_hdr_async(stream, |_req: &Request<()>, mut res: Response<()>| {
            res.headers_mut()
                .insert("fp-conn-id", HeaderValue::from_static("conn-id"));
            Ok(res)
        })
        .await
        .unwrap();
        // Ignore the data sources message
        ws.next().await.unwrap().unwrap();

        // Send two queries
        let op_1 = Base64Uuid::parse_str("10000000-0000-0000-0000-000000000000").unwrap();
        let message_1 = ServerMessage::new_invoke_proxy_request(
            rmp_serde::to_vec(&ProviderRequest::builder()
                .query_type(TIMESERIES_QUERY_TYPE)
                .query_data(Blob::builder()
                                .data(
                        b"query=query1&time_range=2022-08-31T11:00:00.000Z+2022-08-31T12:00:00.000Z"
                            .to_vec()
                                )
                                .mime_type("application/x-www-form-urlencoded").build()
                )
                .config(Value::Null)
                .build())
            .unwrap(),
            Name::from_static("prometheus-dev"),
            2,
            op_1,
        )
        .serialize_msgpack();

        ws.send(Message::Binary(message_1)).await.unwrap();

        let op_2 = Base64Uuid::parse_str("20000000-0000-0000-0000-000000000000").unwrap();
        let message_2 = ServerMessage::new_invoke_proxy_request(
            rmp_serde::to_vec(&ProviderRequest::builder()
                .query_type(TIMESERIES_QUERY_TYPE)
                .query_data(Blob::builder()
                                .data(
                        b"query=query2&time_range=2022-08-31T11:00:00.000Z+2022-08-31T12:00:00.000Z"
                            .to_vec()
                                ).
                    mime_type("application/x-www-form-urlencoded")
                    .build())
                .config(Value::Null)
                .build())
            .unwrap(),
            Name::from_static("prometheus-dev"),
            2,
            op_2,
        )
        .serialize_msgpack();
        ws.send(Message::Binary(message_2)).await.unwrap();

        // Parse the query result
        if let Message::Binary(message) = ws.next().await.unwrap().unwrap() {
            let message = ProxyMessage::deserialize_msgpack(message).unwrap();
            // Check that the second query comes back first
            assert_eq!(message.op_id.unwrap(), op_2);
            assert!(matches!(
                message.payload,
                ProxyMessagePayload::InvokeProxyResponse(_)
            ));

            // Now we will wait for the first query
            if let Message::Binary(message) = ws.next().await.unwrap().unwrap() {
                let message = ProxyMessage::deserialize_msgpack(message).unwrap();
                assert_eq!(message.op_id.unwrap(), op_1);
                assert!(matches!(
                    message.payload,
                    ProxyMessagePayload::InvokeProxyResponse(_)
                ));
                return;
                // Everything is fine so just stop handle_connection
            }
        }

        panic!("received the wrong response type or wrong order");
    };

    let (tx, _) = broadcast::channel(3);
    select! {
      result = service.connect(tx).fuse() => result.unwrap(),
      _ = handle_connection.fuse() => {}
    }
}

#[test(tokio::test)]
async fn calls_provider_with_query_and_sends_error() {
    let (prometheus, data_sources) = mock_prometheus().await;
    let prometheus_response = r#"{
        "status" : "success",
        "data" : {
        "resultType" : "vector",
        "result" : []
        }
    }"#;
    let status_mock = prometheus.mock(|when, then| {
        when.path("/api/v1/query");
        then.status(200).body(prometheus_response);
    });
    let query_mock = prometheus.mock(|when, then| {
        when.path("/api/v1/query_range");
        then.status(418).body("Some error");
    });

    // Create a websocket listener for the proxy to connect to
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let service = ProxyService::init(
        format!("ws://{addr}").parse().unwrap(),
        TOKEN.clone(),
        Path::new("./providers"),
        data_sources,
        5,
        None,
        Duration::from_secs(300),
    )
    .await;

    // After the proxy connects, send it a query
    let handle_connection = async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_hdr_async(stream, |_req: &Request<()>, mut res: Response<()>| {
            res.headers_mut()
                .insert("fp-conn-id", HeaderValue::from_static("conn-id"));
            Ok(res)
        })
        .await
        .unwrap();
        ws.next().await.unwrap().unwrap();

        // Send query
        let op_id = Base64Uuid::new();
        let request = ProviderRequest::builder()
            .query_type(TIMESERIES_QUERY_TYPE)
            .query_data(Blob::builder()
                            .data(b"query=test%20query&time_range=2022-08-31T11:00:00.000Z+2022-08-31T12:00:00.000Z".to_vec())
                            .mime_type("application/x-www-form-urlencoded")
                .build())
            .config(Value::Null)
            .build();
        let message = ServerMessage::new_invoke_proxy_request(
            rmp_serde::to_vec(&request).unwrap(),
            Name::from_static("prometheus-dev"),
            2,
            op_id,
        );
        let message = message.serialize_msgpack();
        ws.send(Message::Binary(message)).await.unwrap();

        // Parse the query result
        let response = ws.next().await.unwrap().unwrap();
        let response = match response {
            Message::Binary(message) => ProxyMessage::deserialize_msgpack(message).unwrap(),
            _ => panic!("wrong response message type"),
        };
        let result = match response.payload {
            ProxyMessagePayload::InvokeProxyResponse(message) => message,
            other => panic!("wrong message type: {:?}", other),
        };
        assert_eq!(response.op_id.unwrap(), op_id);
        let result: Result<Blob, Error> = rmp_serde::from_slice(&result.data).unwrap();
        status_mock.assert_hits(1);
        query_mock.assert_hits(1);
        assert!(matches!(result, Err(Error::Http { .. })));
    };

    let (tx, _) = broadcast::channel(3);
    select! {
      result = service.connect(tx).fuse() => result.unwrap(),
      _ = handle_connection.fuse() => {}
    }
}

#[test(tokio::test)]
async fn reconnects_if_websocket_closes() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let service = ProxyService::new(
        format!("ws://{addr}").parse().unwrap(),
        TOKEN.clone(),
        WasmModules::new(),
        Default::default(),
        1,
        None,
        Duration::from_secs(300),
    );

    tokio::time::pause();

    let handle_connection = async move {
        let (stream, _) = listener.accept().await.unwrap();
        let ws = accept_hdr_async(stream, |_req: &Request<()>, _res: Response<()>| {
            Err(Response::builder().status(500).body(None).unwrap())
        })
        .await
        .ok();

        drop(ws);

        tokio::time::advance(tokio::time::Duration::from_secs(45)).await;
        tokio::time::resume();

        // Proxy should try to reconnect
        let (stream, _) = listener.accept().await.unwrap();
        let ws = accept_hdr_async(stream, |_req: &Request<()>, _res: Response<()>| {
            Err(Response::builder().status(500).body(None).unwrap())
        })
        .await
        .ok();

        drop(ws);

        let (_stream, _) = listener.accept().await.unwrap();
        panic!("should not get here because it should not try again");
    };

    let (tx, _) = broadcast::channel(3);
    let result = select! {
      result = service.connect(tx).fuse() => result,
      _ = handle_connection.fuse() => unreachable!()
    };
    assert_eq!(
        format!("{}", result.unwrap_err()),
        "HTTP error: 500 Internal Server Error"
    );
}

#[test(tokio::test)]
async fn service_shutdown() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let service = ProxyService::new(
        format!("ws://{addr}").parse().unwrap(),
        TOKEN.clone(),
        WasmModules::new(),
        Default::default(),
        1,
        None,
        Duration::from_secs(300),
    );

    let (tx, _) = broadcast::channel(3);
    let tx_clone = tx.clone();
    let handle_connection = async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_hdr_async(stream, |_req: &Request<()>, mut res: Response<()>| {
            res.headers_mut()
                .insert("fp-conn-id", HeaderValue::from_static("conn-id"));

            Ok(res)
        })
        .await
        .unwrap();

        // Signal the service to actually shutdown (the sleep is to ensure that
        // the service is able to spawn the read/write loops).
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(tx_clone.send(()).is_ok());

        // Read any message sent from the service, until it gets closed, which
        // will indicate that the service has shutdown.
        loop {
            if (ws.next().await).is_none() {
                break;
            };
        }
    };

    // Wait for both the service and our test handle_connection are stopped
    let (_, result) = join!(handle_connection, service.connect(tx));
    if let Err(err) = result {
        panic!("unexpected error occurred: {:?}", err);
    }
}
