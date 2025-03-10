#[macro_use]
extern crate log;

mod backend;
mod error;
mod utils;

use anyhow::Result;
use chat_prompts::{MergeRagContextPolicy, PromptTemplateType};
use clap::{ArgGroup, Parser};
use error::ServerError;
use hyper::{
    body::HttpBody,
    header,
    server::conn::AddrStream,
    service::{make_service_fn, service_fn},
    Body, Request, Response, Server, StatusCode,
};
use llama_core::metadata::ggml::GgmlMetadataBuilder;
use once_cell::sync::OnceCell;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fmt, net::SocketAddr, path::PathBuf};
use tokio::{net::TcpListener, sync::RwLock};
use utils::{is_valid_url, LogLevel};

type Error = Box<dyn std::error::Error + Send + Sync + 'static>;

// global system prompt
pub(crate) static GLOBAL_RAG_PROMPT: OnceCell<String> = OnceCell::new();
// server info
pub(crate) static SERVER_INFO: OnceCell<RwLock<ServerInfo>> = OnceCell::new();
// API key
pub(crate) static LLAMA_API_KEY: OnceCell<String> = OnceCell::new();
// Global context window used for setting the max number of user messages for the retrieval
pub(crate) static CONTEXT_WINDOW: OnceCell<u64> = OnceCell::new();
// Global keyword search configuration
pub(crate) static KW_SEARCH_CONFIG: OnceCell<KeywordSearchConfig> = OnceCell::new();

// default port
const DEFAULT_PORT: &str = "8080";

#[derive(Clone, Debug)]
pub struct AppState {
    pub state_thing: String,
}

#[derive(Debug, Parser)]
#[command(name = "LlamaEdge-RAG API Server", version = env!("CARGO_PKG_VERSION"), author = env!("CARGO_PKG_AUTHORS"), about = "LlamaEdge-RAG API Server")]
#[command(group = ArgGroup::new("socket_address_group").multiple(false).args(&["socket_addr", "port"]))]
struct Cli {
    /// Sets names for chat and embedding models. The names are separated by comma without space, for example, '--model-name Llama-2-7b,all-minilm'.
    #[arg(short, long, value_delimiter = ',', required = true)]
    model_name: Vec<String>,
    /// Model aliases for chat and embedding models
    #[arg(
        short = 'a',
        long,
        value_delimiter = ',',
        default_value = "default,embedding"
    )]
    model_alias: Vec<String>,
    /// Sets context sizes for chat and embedding models, respectively. The sizes are separated by comma without space, for example, '--ctx-size 4096,384'. The first value is for the chat model, and the second is for the embedding model.
    #[arg(
        short = 'c',
        long,
        value_delimiter = ',',
        default_value = "4096,384",
        value_parser = clap::value_parser!(u64)
    )]
    ctx_size: Vec<u64>,
    /// Sets prompt templates for chat and embedding models, respectively. The prompt templates are separated by comma without space, for example, '--prompt-template llama-2-chat,embedding'. The first value is for the chat model, and the second is for the embedding model.
    #[arg(short, long, value_delimiter = ',', value_parser = clap::value_parser!(PromptTemplateType), required = true)]
    prompt_template: Vec<PromptTemplateType>,
    /// Halt generation at PROMPT, return control.
    #[arg(short, long)]
    reverse_prompt: Option<String>,
    /// Number of tokens to predict, -1 = infinity, -2 = until context filled.
    #[arg(short, long, default_value = "-1")]
    n_predict: i32,
    /// Number of layers to run on the GPU
    #[arg(short = 'g', long, default_value = "100")]
    n_gpu_layers: u64,
    /// Split the model across multiple GPUs. Possible values: `none` (use one GPU only), `layer` (split layers and KV across GPUs, default), `row` (split rows across GPUs)
    #[arg(long, default_value = "layer")]
    split_mode: String,
    /// The main GPU to use.
    #[arg(long)]
    main_gpu: Option<u64>,
    /// How split tensors should be distributed accross GPUs. If None the model is not split; otherwise, a comma-separated list of non-negative values, e.g., "3,2" presents 60% of the data to GPU 0 and 40% to GPU 1.
    #[arg(long)]
    tensor_split: Option<String>,
    /// Number of threads to use during computation
    #[arg(long, default_value = "2")]
    threads: u64,
    /// BNF-like grammar to constrain generations (see samples in grammars/ dir).
    #[arg(long, default_value = "")]
    pub grammar: String,
    /// JSON schema to constrain generations (https://json-schema.org/), e.g. `{}` for any JSON object. For schemas w/ external $refs, use --grammar + example/json_schema_to_grammar.py instead.
    #[arg(long)]
    pub json_schema: Option<String>,
    /// Sets batch sizes for chat and embedding models, respectively. The sizes are separated by comma without space, for example, '--batch-size 128,64'. The first value is for the chat model, and the second is for the embedding model.
    #[arg(short, long, value_delimiter = ',', default_value = "512,512", value_parser = clap::value_parser!(u64))]
    batch_size: Vec<u64>,
    /// Sets physical maximum batch sizes for chat and/or embedding models. To run both chat and embedding models, the sizes should be separated by comma without space, for example, '--ubatch-size 512,512'. The first value is for the chat model, and the second for the embedding model.
    #[arg(short, long, value_delimiter = ',', default_value = "512,512", value_parser = clap::value_parser!(u64))]
    ubatch_size: Vec<u64>,
    /// Custom rag prompt.
    #[arg(long)]
    rag_prompt: Option<String>,
    /// Strategy for merging RAG context into chat messages.
    #[arg(long = "rag-policy", default_value_t, value_enum)]
    policy: MergeRagContextPolicy,
    /// URL of Qdrant REST Service
    #[arg(long, default_value = "http://127.0.0.1:6333")]
    qdrant_url: String,
    /// Name of Qdrant collection
    #[arg(long, default_value = "default", value_delimiter = ',')]
    qdrant_collection_name: Vec<String>,
    /// Max number of retrieved result (no less than 1)
    #[arg(long, default_value = "5", value_delimiter = ',', value_parser = clap::value_parser!(u64))]
    qdrant_limit: Vec<u64>,
    /// Minimal score threshold for the search result
    #[arg(long, default_value = "0.4", value_delimiter = ',', value_parser = clap::value_parser!(f32))]
    qdrant_score_threshold: Vec<f32>,
    /// Maximum number of tokens each chunk contains
    #[arg(long, default_value = "100", value_parser = clap::value_parser!(usize))]
    chunk_capacity: usize,
    /// Maximum number of user messages used in the retrieval
    #[arg(long, default_value = "1", value_parser = clap::value_parser!(u64))]
    context_window: u64,
    /// URL of the keyword search service
    #[arg(long)]
    kw_search_url: Option<String>,
    /// Whether to include usage in the stream response. Defaults to false.
    #[arg(long, default_value = "false")]
    include_usage: bool,
    /// Socket address of LlamaEdge-RAG API Server instance. For example, `0.0.0.0:8080`.
    #[arg(long, default_value = None, value_parser = clap::value_parser!(SocketAddr), group = "socket_address_group")]
    socket_addr: Option<SocketAddr>,
    /// Port number
    #[arg(long, default_value = DEFAULT_PORT, value_parser = clap::value_parser!(u16), group = "socket_address_group")]
    port: u16,
    /// Root path for the Web UI files
    #[arg(long, default_value = "chatbot-ui")]
    web_ui: PathBuf,
    /// Deprecated. Print prompt strings to stdout
    #[arg(long)]
    log_prompts: bool,
    /// Deprecated. Print statistics to stdout
    #[arg(long)]
    log_stat: bool,
    /// Deprecated. Print all log information to stdout
    #[arg(long)]
    log_all: bool,
}

#[allow(clippy::needless_return)]
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), ServerError> {
    let mut plugin_debug = false;

    // get the environment variable `LLAMA_LOG`
    let log_level: LogLevel = std::env::var("LLAMA_LOG")
        .unwrap_or("info".to_string())
        .parse()
        .unwrap_or(LogLevel::Info);

    if log_level == LogLevel::Debug || log_level == LogLevel::Trace {
        plugin_debug = true;
    }
    // set global logger
    wasi_logger::Logger::install().expect("failed to install wasi_logger::Logger");
    log::set_max_level(log_level.into());

    if let Ok(api_key) = std::env::var("API_KEY") {
        // define a const variable for the API key
        if let Err(e) = LLAMA_API_KEY.set(api_key) {
            let err_msg = format!("Failed to set API key. {}", e);

            error!(target: "stdout", "{}", err_msg);

            return Err(ServerError::Operation(err_msg));
        }
    }

    // parse the command line arguments
    let cli = Cli::parse();

    info!(target: "stdout", "log_level: {}", log_level);

    // log the version of the server
    info!(target: "stdout", "server_version: {}", env!("CARGO_PKG_VERSION"));

    // log model name
    if cli.model_name.len() != 2 {
        return Err(ServerError::ArgumentError(
            "LlamaEdge RAG API server requires a chat model and an embedding model.".to_owned(),
        ));
    }
    info!(target: "stdout", "model_name: {}", cli.model_name.join(","));

    // log model alias
    if cli.model_alias.len() != 2 {
        return Err(ServerError::ArgumentError(
            "LlamaEdge RAG API server requires two model aliases: one for chat model, one for embedding model.".to_owned(),
        ));
    }
    info!(target: "stdout", "model_alias: {}", cli.model_alias.join(","));

    // log context size
    if cli.ctx_size.len() != 2 {
        return Err(ServerError::ArgumentError(
            "LlamaEdge RAG API server requires two context sizes: one for chat model, one for embedding model.".to_owned(),
        ));
    }
    let ctx_sizes_str: String = cli
        .ctx_size
        .iter()
        .map(|n| n.to_string())
        .collect::<Vec<String>>()
        .join(",");
    info!(target: "stdout", "ctx_size: {}", ctx_sizes_str);

    // log batch size
    if cli.batch_size.len() != 2 {
        return Err(ServerError::ArgumentError(
            "LlamaEdge RAG API server requires two batch sizes: one for chat model, one for embedding model.".to_owned(),
        ));
    }
    let batch_sizes_str: String = cli
        .batch_size
        .iter()
        .map(|n| n.to_string())
        .collect::<Vec<String>>()
        .join(",");
    info!(target: "stdout", "batch_size: {}", batch_sizes_str);

    // log ubatch size
    if cli.ubatch_size.len() != 2 {
        return Err(ServerError::ArgumentError(
            "LlamaEdge RAG API server requires two ubatch sizes: one for chat model, one for embedding model.".to_owned(),
        ));
    }
    let ubatch_sizes_str: String = cli
        .ubatch_size
        .iter()
        .map(|n| n.to_string())
        .collect::<Vec<String>>()
        .join(",");
    info!(target: "stdout", "ubatch_size: {}", ubatch_sizes_str);

    // log prompt template
    if cli.prompt_template.len() != 2 {
        return Err(ServerError::ArgumentError(
            "LlamaEdge RAG API server requires two prompt templates: one for chat model, one for embedding model.".to_owned(),
        ));
    }
    let prompt_template_str: String = cli
        .prompt_template
        .iter()
        .map(|n| n.to_string())
        .collect::<Vec<String>>()
        .join(",");
    info!(target: "stdout", "prompt_template: {}", prompt_template_str);

    // log reverse prompt
    if let Some(reverse_prompt) = &cli.reverse_prompt {
        info!(target: "stdout", "reverse_prompt: {}", reverse_prompt);
    }

    // log n_predict
    info!(target: "stdout", "n_predict: {}", &cli.n_predict);

    // log n_gpu_layers
    info!(target: "stdout", "n_gpu_layers: {}", &cli.n_gpu_layers);

    // log split_mode
    info!(target: "stdout", "split_mode: {}", cli.split_mode);

    // log main GPU
    if let Some(main_gpu) = &cli.main_gpu {
        info!(target: "stdout", "main_gpu: {}", main_gpu);
    }

    // log tensor split
    if let Some(tensor_split) = &cli.tensor_split {
        info!(target: "stdout", "tensor_split: {}", tensor_split);
    }

    // log threads
    info!(target: "stdout", "threads: {}", cli.threads);

    // log grammar
    if !cli.grammar.is_empty() {
        info!(target: "stdout", "grammar: {}", &cli.grammar);
    }

    // log json schema
    if let Some(json_schema) = &cli.json_schema {
        info!(target: "stdout", "json_schema: {}", json_schema);
    }

    // log rag prompt
    if let Some(rag_prompt) = &cli.rag_prompt {
        info!(target: "stdout", "rag_prompt: {}", rag_prompt);

        GLOBAL_RAG_PROMPT.set(rag_prompt.clone()).map_err(|_| {
            ServerError::Operation("Failed to set `GLOBAL_RAG_PROMPT`.".to_string())
        })?;
    }

    // log qdrant url
    if !is_valid_url(&cli.qdrant_url) {
        let err_msg = format!(
            "The URL of Qdrant REST API is invalid: {}.",
            &cli.qdrant_url
        );

        // log
        {
            error!(target: "stdout", "rag_prompt: {}", err_msg);
        }

        return Err(ServerError::ArgumentError(err_msg));
    }
    info!(target: "stdout", "qdrant_url: {}", &cli.qdrant_url);

    if cli.qdrant_collection_name.len() != cli.qdrant_limit.len()
        && cli.qdrant_limit.len() > 1
        && cli.qdrant_score_threshold.len() > 1
    {
        return Err(ServerError::ArgumentError(
            "LlamaEdge RAG API server requires the same number of Qdrant collection names and limits; or the limit is only one value for all collections.".to_owned(),
        ));
    }

    if cli.qdrant_collection_name.len() != cli.qdrant_score_threshold.len()
        && cli.qdrant_score_threshold.len() > 1
        && cli.qdrant_score_threshold.len() > 1
    {
        return Err(ServerError::ArgumentError(
            "LlamaEdge RAG API server requires the same number of Qdrant collection names and score thresholds; or the score threshold is only one value for all collections.".to_owned(),
        ));
    }

    // log qdrant collection name
    let qdrant_collection_name_str: String = cli
        .qdrant_collection_name
        .iter()
        .map(|n| n.to_string())
        .collect::<Vec<String>>()
        .join(",");
    info!(target: "stdout", "qdrant_collection_name: {}", qdrant_collection_name_str);

    // log qdrant limit
    let qdrant_limit_str: String = cli
        .qdrant_limit
        .iter()
        .map(|n| n.to_string())
        .collect::<Vec<String>>()
        .join(",");
    info!(target: "stdout", "qdrant_limit: {}", qdrant_limit_str);

    // log qdrant score threshold
    let qdrant_score_threshold_str: String = cli
        .qdrant_score_threshold
        .iter()
        .map(|n| n.to_string())
        .collect::<Vec<String>>()
        .join(",");
    info!(target: "stdout", "qdrant_score_threshold: {}", qdrant_score_threshold_str);

    // create qdrant config
    let mut qdrant_config_vec: Vec<QdrantConfig> = Vec::new();
    for (idx, col_name) in cli.qdrant_collection_name.iter().enumerate() {
        let limit = if cli.qdrant_limit.len() == 1 {
            cli.qdrant_limit[0]
        } else {
            cli.qdrant_limit[idx]
        };

        let score_threshold = if cli.qdrant_score_threshold.len() == 1 {
            cli.qdrant_score_threshold[0]
        } else {
            cli.qdrant_score_threshold[idx]
        };

        let qdrant_config = QdrantConfig {
            url: cli.qdrant_url.clone(),
            collection_name: col_name.clone(),
            limit,
            score_threshold,
        };

        qdrant_config_vec.push(qdrant_config);
    }

    // log chunk capacity
    info!(target: "stdout", "chunk_capacity: {}", &cli.chunk_capacity);

    // log context window
    info!(target: "stdout", "context_window: {}", &cli.context_window);
    CONTEXT_WINDOW
        .set(cli.context_window)
        .map_err(|e| ServerError::Operation(format!("Failed to set `CONTEXT_WINDOW`. {}", e)))?;

    // RAG policy
    info!(target: "stdout", "rag_policy: {}", &cli.policy);

    let mut policy = cli.policy;
    if policy == MergeRagContextPolicy::SystemMessage && !cli.prompt_template[0].has_system_prompt()
    {
        warn!(target: "server_config", "{}", format!("The chat model does not support system message, while the '--policy' option sets to \"{}\". Update the RAG policy to {}.", cli.policy, MergeRagContextPolicy::LastUserMessage));

        policy = MergeRagContextPolicy::LastUserMessage;
    }

    // keyword search configuration
    if let Some(kw_search_url) = &cli.kw_search_url {
        let kw_search_config = KeywordSearchConfig {
            url: kw_search_url.clone(),
        };
        KW_SEARCH_CONFIG.set(kw_search_config).unwrap();
    }

    // log include_usage
    info!(target: "stdout", "include_usage: {}", cli.include_usage);

    // create metadata for chat model
    let chat_metadata = GgmlMetadataBuilder::new(
        cli.model_name[0].clone(),
        cli.model_alias[0].clone(),
        cli.prompt_template[0],
    )
    .with_ctx_size(cli.ctx_size[0])
    .with_reverse_prompt(cli.reverse_prompt)
    .with_batch_size(cli.batch_size[0])
    .with_ubatch_size(cli.ubatch_size[0])
    .with_n_predict(cli.n_predict)
    .with_n_gpu_layers(cli.n_gpu_layers)
    .with_split_mode(cli.split_mode.clone())
    .with_main_gpu(cli.main_gpu)
    .with_tensor_split(cli.tensor_split.clone())
    .with_threads(cli.threads)
    .with_grammar(cli.grammar)
    .with_json_schema(cli.json_schema)
    .enable_plugin_log(true)
    .enable_debug_log(plugin_debug)
    .include_usage(cli.include_usage)
    .build();

    let chat_model_info = ModelConfig {
        name: chat_metadata.model_name.clone(),
        ty: "chat".to_string(),
        prompt_template: chat_metadata.prompt_template,
        n_predict: chat_metadata.n_predict,
        reverse_prompt: chat_metadata.reverse_prompt.clone(),
        n_gpu_layers: chat_metadata.n_gpu_layers,
        ctx_size: chat_metadata.ctx_size,
        batch_size: chat_metadata.batch_size,
        ubatch_size: chat_metadata.ubatch_size,
        temperature: chat_metadata.temperature,
        top_p: chat_metadata.top_p,
        repeat_penalty: chat_metadata.repeat_penalty,
        presence_penalty: chat_metadata.presence_penalty,
        frequency_penalty: chat_metadata.frequency_penalty,
        split_mode: chat_metadata.split_mode.clone(),
        main_gpu: chat_metadata.main_gpu,
        tensor_split: chat_metadata.tensor_split.clone(),
    };

    // chat model
    let chat_models = [chat_metadata];

    // create metadata for embedding model
    let embedding_metadata = GgmlMetadataBuilder::new(
        cli.model_name[1].clone(),
        cli.model_alias[1].clone(),
        cli.prompt_template[1],
    )
    .with_ctx_size(cli.ctx_size[1])
    .with_batch_size(cli.batch_size[1])
    .with_ubatch_size(cli.ubatch_size[1])
    .with_split_mode(cli.split_mode)
    .with_main_gpu(cli.main_gpu)
    .with_tensor_split(cli.tensor_split)
    .with_threads(cli.threads)
    .enable_plugin_log(true)
    .enable_debug_log(plugin_debug)
    .build();

    let embedding_model_info = ModelConfig {
        name: embedding_metadata.model_name.clone(),
        ty: "embedding".to_string(),
        ctx_size: embedding_metadata.ctx_size,
        batch_size: embedding_metadata.batch_size,
        ubatch_size: embedding_metadata.ubatch_size,
        prompt_template: embedding_metadata.prompt_template,
        n_predict: embedding_metadata.n_predict,
        reverse_prompt: embedding_metadata.reverse_prompt.clone(),
        n_gpu_layers: embedding_metadata.n_gpu_layers,
        temperature: embedding_metadata.temperature,
        top_p: embedding_metadata.top_p,
        repeat_penalty: embedding_metadata.repeat_penalty,
        presence_penalty: embedding_metadata.presence_penalty,
        frequency_penalty: embedding_metadata.frequency_penalty,
        split_mode: embedding_metadata.split_mode.clone(),
        main_gpu: embedding_metadata.main_gpu,
        tensor_split: embedding_metadata.tensor_split.clone(),
    };

    // embedding model
    let embedding_models = [embedding_metadata];

    // create rag config
    let rag_config = RagConfig {
        chat_model: chat_model_info,
        embedding_model: embedding_model_info,
        policy,
    };

    // initialize the core context
    llama_core::init_ggml_rag_context(&chat_models[..], &embedding_models[..]).map_err(|e| {
        let err_msg = format!("Failed to initialize the core context. {}", e);

        // log
        error!(target: "stdout", "{}", &err_msg);

        ServerError::Operation(err_msg)
    })?;

    // get the plugin version info
    let plugin_info =
        llama_core::get_plugin_info().map_err(|e| ServerError::Operation(e.to_string()))?;
    let plugin_version = format!(
        "b{build_number} (commit {commit_id})",
        build_number = plugin_info.build_number,
        commit_id = plugin_info.commit_id,
    );

    // log plugin version
    info!(target: "stdout", "plugin_ggml_version: {}", &plugin_version);

    // socket address
    let addr = match cli.socket_addr {
        Some(addr) => addr,
        None => SocketAddr::from(([0, 0, 0, 0], cli.port)),
    };
    let port = addr.port().to_string();

    // get the environment variable `NODE_VERSION`
    // Note that this is for satisfying the requirement of `gaianet-node` project.
    let node = std::env::var("NODE_VERSION").ok();
    if node.is_some() {
        // log node version
        info!(target: "stdout", "gaianet_node_version: {}", node.as_ref().unwrap());
    }

    // create server info
    let server_info = ServerInfo {
        node,
        server: ApiServer {
            ty: "rag".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            plugin_version,
            port,
        },
        rag_config,
        qdrant_config: qdrant_config_vec,
        extras: HashMap::new(),
    };
    SERVER_INFO
        .set(RwLock::new(server_info))
        .map_err(|_| ServerError::Operation("Failed to set `SERVER_INFO`.".to_string()))?;

    let new_service = make_service_fn(move |conn: &AddrStream| {
        // log socket address
        info!(target: "stdout", "remote_addr: {}, local_addr: {}", conn.remote_addr().to_string(), conn.local_addr().to_string());

        let web_ui = cli.web_ui.to_string_lossy().to_string();
        let chunk_capacity = cli.chunk_capacity;

        async move {
            Ok::<_, Error>(service_fn(move |req| {
                handle_request(req, chunk_capacity, web_ui.clone())
            }))
        }
    });

    let tcp_listener = TcpListener::bind(addr).await.unwrap();
    info!(target: "stdout", "Listening on {}", addr);

    let server = Server::from_tcp(tcp_listener.into_std().unwrap())
        .unwrap()
        .serve(new_service);

    match server.await {
        Ok(_) => Ok(()),
        Err(e) => Err(ServerError::Operation(e.to_string())),
    }
}

async fn handle_request(
    req: Request<Body>,
    chunk_capacity: usize,
    web_ui: String,
) -> Result<Response<Body>, hyper::Error> {
    let path_str = req.uri().path();
    let path_buf = PathBuf::from(path_str);
    let mut path_iter = path_buf.iter();
    path_iter.next(); // Must be Some(OsStr::new(&path::MAIN_SEPARATOR.to_string()))
    let root_path = path_iter.next().unwrap_or_default();
    let root_path = "/".to_owned() + root_path.to_str().unwrap_or_default();

    // check if the API key is valid
    if let Some(auth_header) = req.headers().get("authorization") {
        if !auth_header.is_empty() {
            let auth_header = match auth_header.to_str() {
                Ok(auth_header) => auth_header,
                Err(e) => {
                    let err_msg = format!("Failed to get authorization header: {}", e);
                    return Ok(error::unauthorized(err_msg));
                }
            };

            let api_key = auth_header.split(" ").nth(1).unwrap_or_default();
            info!(target: "stdout", "API Key: {}", api_key);

            if let Some(stored_api_key) = LLAMA_API_KEY.get() {
                if api_key != stored_api_key {
                    let err_msg = "Invalid API key.";
                    return Ok(error::unauthorized(err_msg));
                }
            }
        }
    }

    // log request
    {
        let method = hyper::http::Method::as_str(req.method()).to_string();
        let path = req.uri().path().to_string();
        let version = format!("{:?}", req.version());
        if req.method() == hyper::http::Method::POST {
            let size: u64 = match req.headers().get("content-length") {
                Some(content_length) => content_length.to_str().unwrap().parse().unwrap(),
                None => 0,
            };

            info!(target: "stdout", "method: {}, http_version: {}, content-length: {}", method, version, size);
            info!(target: "stdout", "endpoint: {}", path);
        } else {
            info!(target: "stdout", "method: {}, http_version: {}", method, version);
            info!(target: "stdout", "endpoint: {}", path);
        }
    }

    let response = match root_path.as_str() {
        "/echo" => Response::new(Body::from("echo test")),
        "/v1" => backend::handle_llama_request(req, chunk_capacity).await,
        _ => static_response(path_str, web_ui),
    };

    // log response
    {
        let status_code = response.status();
        if status_code.as_u16() < 400 {
            // log response
            let response_version = format!("{:?}", response.version());
            info!(target: "stdout", "response_version: {}", response_version);
            let response_body_size: u64 = response.body().size_hint().lower();
            info!(target: "stdout", "response_body_size: {}", response_body_size);
            let response_status = status_code.as_u16();
            info!(target: "stdout", "response_status: {}", response_status);
            let response_is_success = status_code.is_success();
            info!(target: "stdout", "response_is_success: {}", response_is_success);
        } else {
            let response_version = format!("{:?}", response.version());
            error!(target: "stdout", "response_version: {}", response_version);
            let response_body_size: u64 = response.body().size_hint().lower();
            error!(target: "stdout", "response_body_size: {}", response_body_size);
            let response_status = status_code.as_u16();
            error!(target: "stdout", "response_status: {}", response_status);
            let response_is_success = status_code.is_success();
            error!(target: "stdout", "response_is_success: {}", response_is_success);
            let response_is_client_error = status_code.is_client_error();
            error!(target: "stdout", "response_is_client_error: {}", response_is_client_error);
            let response_is_server_error = status_code.is_server_error();
            error!(target: "stdout", "response_is_server_error: {}", response_is_server_error);
        }
    }

    Ok(response)
}

fn static_response(path_str: &str, root: String) -> Response<Body> {
    let path = match path_str {
        "/" => "/index.html",
        _ => path_str,
    };

    let mime = mime_guess::from_path(path);

    match std::fs::read(format!("{root}/{path}")) {
        Ok(content) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, mime.first_or_text_plain().to_string())
            .body(Body::from(content))
            .unwrap(),
        Err(_) => {
            let body = Body::from(std::fs::read(format!("{root}/404.html")).unwrap_or_default());
            Response::builder()
                .status(StatusCode::NOT_FOUND)
                .header(header::CONTENT_TYPE, "text/html")
                .body(body)
                .unwrap()
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct QdrantConfig {
    pub(crate) url: String,
    pub(crate) collection_name: String,
    pub(crate) limit: u64,
    pub(crate) score_threshold: f32,
}
impl fmt::Display for QdrantConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "url: {}, collection_name: {}, limit: {}, score_threshold: {}",
            self.url, self.collection_name, self.limit, self.score_threshold
        )
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ModelConfig {
    // model name
    name: String,
    // type: chat or embedding
    #[serde(rename = "type")]
    ty: String,
    pub ctx_size: u64,
    pub batch_size: u64,
    pub ubatch_size: u64,
    pub prompt_template: PromptTemplateType,
    pub n_predict: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reverse_prompt: Option<String>,
    pub n_gpu_layers: u64,
    pub temperature: f64,
    pub top_p: f64,
    pub repeat_penalty: f64,
    pub presence_penalty: f64,
    pub frequency_penalty: f64,
    pub split_mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub main_gpu: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tensor_split: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ServerInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "node_version")]
    node: Option<String>,
    #[serde(rename = "api_server")]
    server: ApiServer,
    #[serde(flatten)]
    rag_config: RagConfig,
    qdrant_config: Vec<QdrantConfig>,
    extras: HashMap<String, String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ApiServer {
    #[serde(rename = "type")]
    ty: String,
    version: String,
    #[serde(rename = "ggml_plugin_version")]
    plugin_version: String,
    port: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct RagConfig {
    pub chat_model: ModelConfig,
    pub embedding_model: ModelConfig,
    #[serde(rename = "rag_policy")]
    pub policy: MergeRagContextPolicy,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct KeywordSearchConfig {
    pub url: String,
}
