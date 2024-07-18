//! JsonRPC client implementation.
//!
//! TODO: - Add option for http connector.

use http::Uri;
use hyper::{http, Body, Client, Request};
use hyper_tls::HttpsConnector;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::atomic::{AtomicI32, Ordering};

use super::primitives::{
    BestBlockHashResponse, GetBalanceResponse, GetBlockResponse, GetBlockchainInfoResponse,
    GetInfoResponse, GetSubtreesResponse, GetTransactionResponse, GetTreestateResponse,
    GetUtxosResponse, SendTransactionResponse, TxidsResponse,
};

#[derive(Serialize, Deserialize, Debug)]
struct RpcRequest<T> {
    jsonrpc: String,
    method: String,
    params: T,
    id: i32,
}

#[derive(Serialize, Deserialize, Debug)]
struct RpcResponse<T> {
    id: i32,
    jsonrpc: Option<String>,
    result: T,
    error: Option<RpcError>,
}

#[derive(Serialize, Deserialize, Debug)]
struct RpcError {
    code: i32,
    message: String,
    data: Option<Value>,
}

/// General error type for handling JsonRpcConnector errors.
#[derive(Debug, thiserror::Error)]
pub enum JsonRpcConnectorError {
    /// Uncatogorized Errors.
    #[error("{0}")]
    CustomError(String),

    /// Serialization/Deserialization Errors.
    #[error("Serialization/Deserialization Error: {0}")]
    SerdeJsonError(#[from] serde_json::Error),

    /// HTTP Request Errors.
    #[error("HTTP Request Error: {0}")]
    HyperError(#[from] hyper::Error),

    ///HTTP Errors.
    #[error("HTTP Error: {0}")]
    HttpError(#[from] http::Error),

    /// Invalid URI Errors.
    #[error("Invalid URI: {0}")]
    InvalidUriError(#[from] http::uri::InvalidUri),

    /// UTF-8 Conversion Errors.
    #[error("UTF-8 Conversion Error")]
    Utf8Error(#[from] std::string::FromUtf8Error),

    /// Request Timeout Errors.
    #[error("Request Timeout Error")]
    TimeoutError(#[from] tokio::time::error::Elapsed),
}

impl JsonRpcConnectorError {
    /// Constructor for errors without an underlying source
    pub fn new(msg: impl Into<String>) -> Self {
        JsonRpcConnectorError::CustomError(msg.into())
    }

    /// Maps JsonRpcConnectorError to tonic::Status
    pub fn to_grpc_status(&self) -> tonic::Status {
        eprintln!("@zingoproxyd: Error occurred: {}.", self);

        match self {
            JsonRpcConnectorError::SerdeJsonError(_) => {
                tonic::Status::invalid_argument(self.to_string())
            }
            JsonRpcConnectorError::HyperError(_) => tonic::Status::unavailable(self.to_string()),
            JsonRpcConnectorError::HttpError(_) => tonic::Status::internal(self.to_string()),
            _ => tonic::Status::internal(self.to_string()),
        }
    }
}

impl From<JsonRpcConnectorError> for tonic::Status {
    fn from(err: JsonRpcConnectorError) -> Self {
        err.to_grpc_status()
    }
}

/// JsonRPC Client config data.
pub struct JsonRpcConnector {
    uri: http::Uri,
    id_counter: AtomicI32,
    user: Option<String>,
    password: Option<String>,
}

impl JsonRpcConnector {
    /// Returns a new JsonRpcConnector instance, tests uri and returns error if connection is not established.
    pub async fn new(uri: http::Uri, user: Option<String>, password: Option<String>) -> Self {
        Self {
            uri,
            id_counter: AtomicI32::new(0),
            user,
            password,
        }
    }

    /// Returns the uri the JsonRpcConnector is configured to send requests to.
    pub fn uri(&self) -> &Uri {
        &self.uri
    }

    /// Sends a jsonRPC request and returns the response.
    ///
    /// TODO: This function currently resends the call up to 5 times on a server response of "Work queue depth exceeded".
    /// This is because the node's queue can become overloaded and stop servicing RPCs.
    /// This functionality is weak and should be incorporated in Zingo-Proxy's queue mechanism [WIP] that handles various errors appropriately.
    pub async fn send_request<T: Serialize, R: for<'de> Deserialize<'de>>(
        &self,
        method: &str,
        params: T,
    ) -> Result<R, JsonRpcConnectorError> {
        let id = self.id_counter.fetch_add(1, Ordering::SeqCst);
        let req = RpcRequest {
            jsonrpc: "2.0".to_string(),
            method: method.to_string(),
            params,
            id,
        };
        let max_attempts = 5;
        let mut attempts = 0;
        loop {
            attempts += 1;
            let client = Client::builder().build(HttpsConnector::new());
            let mut request_builder = Request::builder()
                .method("POST")
                .uri(self.uri.clone())
                .header("Content-Type", "application/json");
            if let (Some(user), Some(password)) = (&self.user, &self.password) {
                let auth = base64::encode(format!("{}:{}", user, password));
                request_builder =
                    request_builder.header("Authorization", format!("Basic {}", auth));
            }
            let request_body = serde_json::to_string(&req)
                .map_err(JsonRpcConnectorError::SerdeJsonError)?;
            let request = request_builder
                .body(Body::from(request_body))
                .map_err(JsonRpcConnectorError::HttpError)?;
            let response = client
                .request(request)
                .await
                .map_err(JsonRpcConnectorError::HyperError)?;
            let body_bytes = hyper::body::to_bytes(response.into_body())
                .await
                .map_err(JsonRpcConnectorError::HyperError)?;

            let body_str = String::from_utf8_lossy(&body_bytes);
            if body_str.contains("Work queue depth exceeded") {
                if attempts >= max_attempts {
                    return Err(JsonRpcConnectorError::new(
                        "Work queue depth exceeded after multiple attempts",
                    ));
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                continue;
            }
            let response: RpcResponse<R> = serde_json::from_slice(&body_bytes)
                .map_err(JsonRpcConnectorError::SerdeJsonError)?;
            return match response.error {
                Some(error) => Err(JsonRpcConnectorError::new(format!(
                    "RPC Error {}: {}",
                    error.code, error.message
                ))),
                None => Ok(response.result),
            };
        }
    }

    /// Returns software information from the RPC server, as a [`GetInfo`] JSON struct.
    ///
    /// zcashd reference: [`getinfo`](https://zcash.github.io/rpc/getinfo.html)
    /// method: post
    /// tags: control
    pub async fn get_info(&self) -> Result<GetInfoResponse, JsonRpcConnectorError> {
        self.send_request::<(), GetInfoResponse>("getinfo", ())
            .await
    }

    /// Returns blockchain state information, as a [`GetBlockChainInfo`] JSON struct.
    ///
    /// zcashd reference: [`getblockchaininfo`](https://zcash.github.io/rpc/getblockchaininfo.html)
    /// method: post
    /// tags: blockchain
    pub async fn get_blockchain_info(
        &self,
    ) -> Result<GetBlockchainInfoResponse, JsonRpcConnectorError> {
        self.send_request::<(), GetBlockchainInfoResponse>("getblockchaininfo", ())
            .await
    }

    /// Returns the total balance of a provided `addresses` in an [`AddressBalance`] instance.
    ///
    /// zcashd reference: [`getaddressbalance`](https://zcash.github.io/rpc/getaddressbalance.html)
    /// method: post
    /// tags: address
    ///
    /// # Parameters
    ///
    /// - `address_strings`: (object, example={"addresses": ["tmYXBYJj1K7vhejSec5osXK2QsGa5MTisUQ"]}) A JSON map with a single entry
    ///     - `addresses`: (array of strings) A list of base-58 encoded addresses.
    ///
    /// NOTE: Currently unused by Zingo-Proxy and untested!
    pub async fn get_address_balance(
        &self,
        addresses: Vec<String>,
    ) -> Result<GetBalanceResponse, JsonRpcConnectorError> {
        let params = vec![serde_json::to_value(addresses)?];
        self.send_request("getaddressbalance", params).await
    }

    /// Sends the raw bytes of a signed transaction to the local node's mempool, if the transaction is valid.
    /// Returns the [`SentTransactionHash`] for the transaction, as a JSON string.
    ///
    /// zcashd reference: [`sendrawtransaction`](https://zcash.github.io/rpc/sendrawtransaction.html)
    /// method: post
    /// tags: transaction
    ///
    /// # Parameters
    ///
    /// - `raw_transaction_hex`: (string, required, example="signedhex") The hex-encoded raw transaction bytes.
    pub async fn send_raw_transaction(
        &self,
        raw_transaction_hex: String,
    ) -> Result<SendTransactionResponse, JsonRpcConnectorError> {
        let params = vec![serde_json::to_value(raw_transaction_hex)?];
        self.send_request("sendrawtransaction", params).await
    }

    /// Returns the requested block by hash or height, as a [`GetBlock`] JSON string.
    /// If the block is not in Zebra's state, returns
    /// [error code `-8`.](https://github.com/zcash/zcash/issues/5758)
    ///
    /// zcashd reference: [`getblock`](https://zcash.github.io/rpc/getblock.html)
    /// method: post
    /// tags: blockchain
    ///
    /// # Parameters
    ///
    /// - `hash_or_height`: (string, required, example="1") The hash or height for the block to be returned.
    /// - `verbosity`: (number, optional, default=1, example=1) 0 for hex encoded data, 1 for a json object, and 2 for json object with transaction data.
    pub async fn get_block(
        &self,
        hash_or_height: String,
        verbosity: Option<u8>,
    ) -> Result<GetBlockResponse, JsonRpcConnectorError> {
        let params = match verbosity {
            Some(v) => vec![
                serde_json::to_value(hash_or_height)?,
                serde_json::to_value(v)?,
            ],
            None => vec![
                serde_json::to_value(hash_or_height)?,
                serde_json::to_value(1)?,
            ],
        };
        self.send_request("getblock", params).await
    }

    /// Returns the hash of the current best blockchain tip block, as a [`GetBlockHash`] JSON string.
    ///
    /// zcashd reference: [`getbestblockhash`](https://zcash.github.io/rpc/getbestblockhash.html)
    /// method: post
    /// tags: blockchain
    ///
    /// NOTE: Currently unused by Zingo-Proxy and untested!
    pub async fn get_best_block_hash(
        &self,
    ) -> Result<BestBlockHashResponse, JsonRpcConnectorError> {
        self.send_request::<(), BestBlockHashResponse>("getbestblockhash", ())
            .await
    }

    /// Returns all transaction ids in the memory pool, as a JSON array.
    ///
    /// zcashd reference: [`getrawmempool`](https://zcash.github.io/rpc/getrawmempool.html)
    /// method: post
    /// tags: blockchain
    pub async fn get_raw_mempool(&self) -> Result<TxidsResponse, JsonRpcConnectorError> {
        self.send_request::<(), TxidsResponse>("getrawmempool", ())
            .await
    }

    /// Returns information about the given block's Sapling & Orchard tree state.
    ///
    /// zcashd reference: [`z_gettreestate`](https://zcash.github.io/rpc/z_gettreestate.html)
    /// method: post
    /// tags: blockchain
    ///
    /// # Parameters
    ///
    /// - `hash | height`: (string, required, example="00000000febc373a1da2bd9f887b105ad79ddc26ac26c2b28652d64e5207c5b5") The block hash or height.
    pub async fn get_treestate(
        &self,
        hash_or_height: String,
    ) -> Result<GetTreestateResponse, JsonRpcConnectorError> {
        let params = vec![serde_json::to_value(hash_or_height)?];
        self.send_request("z_gettreestate", params).await
    }

    /// Returns information about a range of Sapling or Orchard subtrees.
    ///
    /// zcashd reference: [`z_getsubtreesbyindex`](https://zcash.github.io/rpc/z_getsubtreesbyindex.html) - TODO: fix link
    /// method: post
    /// tags: blockchain
    ///
    /// # Parameters
    ///
    /// - `pool`: (string, required) The pool from which subtrees should be returned. Either "sapling" or "orchard".
    /// - `start_index`: (number, required) The index of the first 2^16-leaf subtree to return.
    /// - `limit`: (number, optional) The maximum number of subtree values to return.
    ///
    /// NOTE: Currently unused by Zingo-Proxy and untested!
    pub async fn get_subtrees_by_index(
        &self,
        pool: String,
        start_index: u16,
        limit: Option<u16>,
    ) -> Result<GetSubtreesResponse, JsonRpcConnectorError> {
        let params = match limit {
            Some(v) => vec![
                serde_json::to_value(pool)?,
                serde_json::to_value(start_index)?,
                serde_json::to_value(v)?,
            ],
            None => vec![
                serde_json::to_value(pool)?,
                serde_json::to_value(start_index)?,
            ],
        };
        self.send_request("z_getsubtreesbyindex", params).await
    }

    /// Returns the raw transaction data, as a [`GetRawTransaction`] JSON string or structure.
    ///
    /// zcashd reference: [`getrawtransaction`](https://zcash.github.io/rpc/getrawtransaction.html)
    /// method: post
    /// tags: transaction
    ///
    /// # Parameters
    ///
    /// - `txid`: (string, required, example="mytxid") The transaction ID of the transaction to be returned.
    /// - `verbose`: (number, optional, default=0, example=1) If 0, return a string of hex-encoded data, otherwise return a JSON object.
    pub async fn get_raw_transaction(
        &self,
        txid_hex: String,
        verbose: Option<u8>,
    ) -> Result<GetTransactionResponse, JsonRpcConnectorError> {
        let params = match verbose {
            Some(v) => vec![serde_json::to_value(txid_hex)?, serde_json::to_value(v)?],
            None => vec![serde_json::to_value(txid_hex)?, serde_json::to_value(0)?],
        };

        self.send_request("getrawtransaction", params).await
    }

    /// Returns the transaction ids made by the provided transparent addresses.
    ///
    /// zcashd reference: [`getaddresstxids`](https://zcash.github.io/rpc/getaddresstxids.html)
    /// method: post
    /// tags: address
    ///
    /// # Parameters
    ///
    /// - `request`: (object, required, example={\"addresses\": [\"tmYXBYJj1K7vhejSec5osXK2QsGa5MTisUQ\"], \"start\": 1000, \"end\": 2000}) A struct with the following named fields:
    ///     - `addresses`: (json array of string, required) The addresses to get transactions from.
    ///     - `start`: (numeric, required) The lower height to start looking for transactions (inclusive).
    ///     - `end`: (numeric, required) The top height to stop looking for transactions (inclusive).
    pub async fn get_address_txids(
        &self,
        addresses: Vec<String>,
        start: u32,
        end: u32,
    ) -> Result<TxidsResponse, JsonRpcConnectorError> {
        let params = serde_json::json!({
            "addresses": addresses,
            "start": start,
            "end": end
        });

        self.send_request("getaddresstxids", vec![params]).await
    }

    /// Returns all unspent outputs for a list of addresses.
    ///
    /// zcashd reference: [`getaddressutxos`](https://zcash.github.io/rpc/getaddressutxos.html)
    /// method: post
    /// tags: address
    ///
    /// # Parameters
    ///
    /// - `addresses`: (array, required, example={\"addresses\": [\"tmYXBYJj1K7vhejSec5osXK2QsGa5MTisUQ\"]}) The addresses to get outputs from.
    ///
    /// NOTE: Currently unused by Zingo-Proxy and untested!
    pub async fn get_address_utxos(
        &self,
        addresses: Vec<String>,
    ) -> Result<Vec<GetUtxosResponse>, JsonRpcConnectorError> {
        let params = vec![serde_json::to_value(addresses)?];
        self.send_request("getaddressutxos", params).await
    }
}

/// Tests connection with zebrad / zebrad.
pub async fn test_node_connection(
    uri: Uri,
    user: Option<String>,
    password: Option<String>,
) -> Result<(), JsonRpcConnectorError> {
    let client = Client::builder().build::<_, Body>(HttpsConnector::new());

    let user = user.unwrap_or_else(|| "xxxxxx".to_string());
    let password = password.unwrap_or_else(|| "xxxxxx".to_string());
    let encoded_auth = base64::encode(format!("{}:{}", user, password));

    let request = Request::builder()
        .method("POST")
        .uri(uri.clone())
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Basic {}", encoded_auth))
        .body(Body::from(
            r#"{"jsonrpc":"2.0","method":"getinfo","params":[],"id":1}"#,
        ))
        .map_err(JsonRpcConnectorError::HttpError)?;
    let response =
        tokio::time::timeout(tokio::time::Duration::from_secs(3), client.request(request))
            .await
            .map_err(JsonRpcConnectorError::TimeoutError)??;
    let body_bytes = hyper::body::to_bytes(response.into_body())
        .await
        .map_err(JsonRpcConnectorError::HyperError)?;
    let _response: RpcResponse<serde_json::Value> =
        serde_json::from_slice(&body_bytes).map_err(JsonRpcConnectorError::SerdeJsonError)?;
    Ok(())
}

/// Tries to connect to zebrad/zcashd using IPv4 and IPv6 and returns the correct uri type, exits program with error message if connection cannot be established.
pub async fn test_node_and_return_uri(
    port: &u16,
    user: Option<String>,
    password: Option<String>,
) -> Result<Uri, JsonRpcConnectorError> {
    let ipv4_uri: Uri = format!("http://127.0.0.1:{}", port)
        .parse()
        .map_err(JsonRpcConnectorError::InvalidUriError)?;
    let ipv6_uri: Uri = format!("http://[::1]:{}", port)
        .parse()
        .map_err(JsonRpcConnectorError::InvalidUriError)?;
    let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(500));
    for _ in 0..3 {
        println!("@zingoproxyd: Trying connection on IPv4.");
        match test_node_connection(ipv4_uri.clone(), user.clone(), password.clone()).await {
            Ok(_) => {
                println!(
                    "@zingoproxyd: Connected to node using IPv4 at address {}.",
                    ipv4_uri
                );
                return Ok(ipv4_uri);
            }
            Err(e_ipv4) => {
                eprintln!("@zingoproxyd: Failed to connect to node using IPv4 with error: {}\n@zingoproxyd: Trying connection on IPv6.", e_ipv4);
                match test_node_connection(ipv6_uri.clone(), user.clone(), password.clone()).await {
                    Ok(_) => {
                        println!(
                            "@zingoproxyd: Connected to node using IPv6 at address {}.",
                            ipv6_uri
                        );
                        return Ok(ipv6_uri);
                    }
                    Err(e_ipv6) => {
                        eprintln!("@zingoproxyd: Failed to connect to node using IPv6 with error: {}.\n@zingoproxyd: Connection not established. Retrying..", e_ipv6);
                        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    }
                }
            }
        }
        interval.tick().await;
    }
    eprintln!("@zingoproxyd: Could not establish connection with node. \n@zingoproxyd: Please check config and confirm node is listening at the correct address and the correct authorisation details have been entered. \n@zingoproxyd: Exiting..");
    std::process::exit(1);
}