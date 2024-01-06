use actix_web::{middleware::Logger, web, App, HttpServer};
use aici_abi::toktree::TokTrie;
use aicirt::api::{GetTagsResp, MkModuleReq, MkModuleResp, SetTagsReq};
use anyhow::Result;
use base64::Engine;
use clap::Parser;
use openai::responses::APIError;
use rllm::{
    config::{ModelConfig, SamplingParams},
    iface::{kill_self, AiciRtIface, AsyncCmdChannel},
    seq::RequestOutput,
    util::apply_settings,
    AddRequest, DType, ExpectedGeneration, LoaderArgs, RllmEngine,
};
use std::{
    collections::HashMap,
    fmt::Display,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Instant,
};
use tokio::sync::mpsc::{channel, error::TryRecvError, Receiver, Sender};

mod completion;
mod openai;

#[derive(Clone, Debug)]
pub struct ServerStats {
    pub num_requests: usize,
    pub num_tokens: usize,
    pub start_time: Instant,
}

impl Display for ServerStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "requests: {}; tokens: {}; uptime: {:?}",
            self.num_requests,
            self.num_tokens,
            self.start_time.elapsed()
        )
    }
}

#[derive(Clone)]
pub struct OpenAIServerData {
    pub worker: Arc<Mutex<InferenceWorker>>,
    pub model_config: ModelConfig,
    pub tokenizer: Arc<tokenizers::Tokenizer>,
    pub tok_trie: Arc<TokTrie>,
    pub side_cmd_ch: AsyncCmdChannel,
    pub stats: Arc<Mutex<ServerStats>>,
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    /// Port to serve on (localhost:port)
    #[arg(long, default_value_t = 8080)]
    port: u16,

    /// Set verbose mode (print all requests)
    #[arg(long, default_value_t = false)]
    verbose: bool,

    /// HuggingFace model name; can be also path starting with "./"
    #[arg(short, long)]
    model: String,

    /// HuggingFace model revision; --model foo/bar@revision is also possible
    #[arg(long)]
    revision: Option<String>,

    /// The folder name that contains safetensor weights and json files
    /// (same structure as HuggingFace online)
    #[arg(long)]
    local_weights: Option<String>,

    /// Tokenizer to use; try --tokenizer list to see options
    #[arg(short, long, default_value = "llama")]
    tokenizer: String,

    /// Path to the aicirt binary.
    #[arg(long)]
    aicirt: String,

    /// Size of JSON comm buffer in megabytes
    #[arg(long, default_value = "32")]
    json_size: usize,

    /// Size of binary comm buffer in megabytes
    #[arg(long, default_value = "32")]
    bin_size: usize,

    /// How many milliseconds to spin-wait for a message over IPC and SHM.
    #[arg(long, default_value = "200")]
    busy_wait_time: u64,

    /// Shm/semaphore name prefix
    #[arg(long, default_value = "/aici0-")]
    shm_prefix: String,

    /// Enable nvprof profiling for given engine step
    #[arg(long, default_value_t = 0)]
    profile_step: usize,

    /// Specify which type to use in the model (bf16, f16, f32)
    #[arg(long, default_value = "")]
    dtype: String,

    /// Specify test-cases (expected/*/*.safetensors)
    #[arg(long)]
    test: Vec<String>,

    /// Specify warm-up request (expected/*/*.safetensors or "off")
    #[arg(long, short)]
    warmup: Option<String>,

    /// Exit after processing warmup request
    #[arg(long, default_value_t = false)]
    warmup_only: bool,

    /// Set engine setting; try '--setting help' to list them
    #[arg(long, short, name = "NAME=VALUE")]
    setting: Vec<String>,

    /// Enable daemon mode (log timestamps)
    #[arg(long, default_value_t = false)]
    daemon: bool,
}

#[actix_web::get("/v1/aici_modules/tags")]
async fn get_aici_module_tags(
    data: web::Data<OpenAIServerData>,
) -> Result<web::Json<GetTagsResp>, APIError> {
    let r = data.side_cmd_ch.get_tags().await.map_err(APIError::just_msg)?;
    Ok(web::Json(r))
}

#[actix_web::post("/v1/aici_modules/tags")]
async fn tag_aici_module(
    data: web::Data<OpenAIServerData>,
    body: web::Json<SetTagsReq>,
) -> Result<web::Json<GetTagsResp>, APIError> {
    let r = data
        .side_cmd_ch
        .set_tags(body.0)
        .await
        .map_err(APIError::just_msg)?;
    Ok(web::Json(r))
}

#[actix_web::post("/v1/aici_modules")]
async fn upload_aici_module(
    data: web::Data<OpenAIServerData>,
    body: web::Bytes,
) -> Result<web::Json<MkModuleResp>, APIError> {
    let binary = base64::engine::general_purpose::STANDARD.encode(body);
    let r = data
        .side_cmd_ch
        .mk_module(MkModuleReq {
            binary,
            meta: serde_json::Value::Null,
        })
        .await
        .map_err(APIError::just_msg)?;
    Ok(web::Json(r))
}

#[actix_web::get("/v1/models")]
async fn models(
    data: web::Data<OpenAIServerData>,
) -> Result<web::Json<openai::responses::List<openai::responses::Model>>, APIError> {
    let id = data.model_config.meta.id.clone();
    Ok(web::Json(openai::responses::List::new(vec![
        openai::responses::Model {
            object: "model",
            id,
            created: 946810800,
            owned_by: "owner".to_string(),
        },
    ])))
}

#[actix_web::get("/ws-http-tunnel/info")]
async fn tunnel_info(
    req: actix_web::HttpRequest,
    data: web::Data<OpenAIServerData>,
) -> Result<web::Json<serde_json::Value>, APIError> {
    let name = req
        .headers()
        .get("x-user-id")
        .map_or("(no header)", |v| v.to_str().unwrap_or("(invalid header)"));
    log::info!("user: {:?}", name);
    let url = "https://github.com/microsoft/aici/blob/main/proxy.md";
    let model = &data.model_config.meta.id;
    let stats = data.stats.lock().unwrap().clone();
    let msg = format!(
        r#"
Model: {model}
Stats: {stats}

More info at: {url}"#
    );
    Ok(web::Json(serde_json::json!({
        "msg": msg,
        "connection_string": "AICI_API_BASE=\"{website}/v1/#key={key}\""
    })))
}

pub enum InferenceReq {
    AddRequest(AddRequest),
}

type InferenceResult = Result<RequestOutput>;

pub struct InferenceWorker {
    req_sender: Sender<InferenceReq>,
    running: HashMap<String, Sender<InferenceResult>>,
}

impl InferenceWorker {
    pub fn new() -> (Self, Receiver<InferenceReq>) {
        let (tx, rx) = channel(128);
        let r = Self {
            req_sender: tx,
            running: HashMap::new(),
        };
        (r, rx)
    }
    pub fn add_request(&mut self, req: AddRequest) -> Result<Receiver<InferenceResult>> {
        let (tx, rx) = channel(128);
        let rid = req.request_id.clone();
        self.req_sender.try_send(InferenceReq::AddRequest(req))?;
        self.running.insert(rid, tx);
        Ok(rx)
    }
}

fn inference_loop(
    handle: Arc<Mutex<InferenceWorker>>,
    mut engine: RllmEngine,
    mut recv: Receiver<InferenceReq>,
    stats: Arc<Mutex<ServerStats>>,
    warmup_only: bool,
) {
    loop {
        loop {
            let req = if engine.num_pending_requests() > 0 {
                recv.try_recv()
            } else {
                Ok(recv.blocking_recv().unwrap())
            };
            match req {
                Ok(InferenceReq::AddRequest(req)) => {
                    let id = req.request_id.clone();
                    match engine.queue_request(req) {
                        Ok(_) => {
                            let mut stats = stats.lock().unwrap();
                            stats.num_requests += 1;
                        }
                        Err(e) => {
                            let tx = handle.lock().unwrap().running.remove(&id).unwrap();
                            if let Err(e) = tx.try_send(Err(e)) {
                                log::warn!("failed to send error to client {id}: {e}");
                            }
                        }
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => panic!(),
            }
        }

        let outputs = engine.step().expect("run_model() failed");
        {
            let mut stats = stats.lock().unwrap();
            stats.num_tokens += 1;
        }

        {
            let running = &mut handle.lock().unwrap().running;
            for outp in outputs {
                let id = outp.request_id.clone();
                let tx = if outp.is_final {
                    running.remove(&id)
                } else {
                    running.get(&id).cloned()
                };

                match tx {
                    Some(tx) => {
                        if let Err(e) = tx.try_send(Ok(outp)) {
                            log::warn!("failed to send output to client {id}: {e}");
                            engine.abort_request(&id);
                        }
                    }
                    None => {
                        if id == "warmup" {
                            if outp.is_final {
                                let text = engine.seq_output_text(&outp.seq_outputs[0]).unwrap();
                                log::info!("warmup done: {text:?}");
                                if warmup_only {
                                    log::info!("warmup done; exiting");
                                    kill_self();
                                }
                            }
                        } else {
                            log::warn!("output for unknown request {id}");
                            engine.abort_request(&id);
                        }
                    }
                }
            }
        }
    }
}

fn run_tests(args: &Args, loader_args: LoaderArgs) {
    let mut engine = RllmEngine::load(loader_args).expect("failed to load model");
    let mut tests = args.test.clone();

    while tests.len() > 0 || engine.num_pending_requests() > 0 {
        if let Some(ref t) = tests.pop() {
            let exp = ExpectedGeneration::load(&PathBuf::from(t)).expect("can't load test");
            log::info!(
                "test {t}: {} tokens; {} logits",
                exp.output.len(),
                exp.output[0].logits.len()
            );
            if exp.output.len() > 11 {
                let mut exp2 = exp.clone();
                engine.add_expected_generation(exp, None).unwrap();
                // add a few tokens in one go to test
                exp2.output[6].ff_section_len = 4;
                engine.add_expected_generation(exp2, None).unwrap();
            } else {
                engine.add_expected_generation(exp, None).unwrap();
            }
        }

        engine.step().expect("test step failed");
    }

    if engine.num_errors > 0 {
        log::error!("there were {} errors", engine.num_errors);
        println!("there were {} errors", engine.num_errors);
        std::process::exit(102);
    }
}

fn spawn_inference_loop(
    args: &Args,
    loader_args: LoaderArgs,
    iface: AiciRtIface,
    stats: Arc<Mutex<ServerStats>>,
) -> Arc<Mutex<InferenceWorker>> {
    let (handle, recv) = InferenceWorker::new();
    let handle_res = Arc::new(Mutex::new(handle));
    let handle = handle_res.clone();

    // prep for move
    let profile_step = args.profile_step;
    let warmup = args.warmup.clone();
    let warmup_only = args.warmup_only.clone();

    std::thread::spawn(move || {
        let mut engine = RllmEngine::load(loader_args).expect("failed to load model");
        engine.profile_step_no = profile_step;
        engine.set_aicirt(iface);
        let wid = "warmup".to_string();
        match warmup {
            Some(w) if w == "off" => {}
            Some(w) => {
                let exp = ExpectedGeneration::load(&PathBuf::from(&w)).expect("can't load warmup");
                log::info!(
                    "warmup {w}: {} tokens; {} logits",
                    exp.output.len(),
                    exp.output[0].logits.len()
                );
                engine.add_expected_generation(exp, Some(wid)).unwrap();
            }
            None => {
                engine
                    .add_request(
                        wid,
                        "The ultimate answer to life,",
                        SamplingParams {
                            max_tokens: 10,
                            ..SamplingParams::default()
                        },
                    )
                    .unwrap();
            }
        }
        inference_loop(handle, engine, recv, stats, warmup_only)
    });

    handle_res
}

#[actix_web::main]
async fn main() -> () {
    let mut args = Args::parse();

    aicirt::init_log(if args.daemon {
        aicirt::LogMode::Deamon
    } else {
        aicirt::LogMode::Normal
    })
    .expect("Failed to initialize log");

    match apply_settings(&args.setting) {
        Ok(_) => {}
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(101);
        }
    }

    let dtype = match args.dtype.as_str() {
        "bf16" => Some(DType::BFloat16),
        "f16" => Some(DType::Half),
        "f32" => Some(DType::Float),
        "" => None,
        _ => panic!("invalid dtype; try one of bf16, f16, f32"),
    };

    if args.model.contains('@') {
        let m = args.model.clone();
        let mut parts = m.split('@');
        args.model = parts.next().unwrap().to_string();
        args.revision = Some(parts.next().unwrap().to_string());
    }

    if args.model.starts_with(".") {
        args.local_weights = Some(args.model.clone());
    }

    let mut loader_args = LoaderArgs::default();
    loader_args.model_id = args.model.clone();
    loader_args.revision = args.revision.clone();
    loader_args.local_weights = args.local_weights.clone();
    loader_args.tokenizer = args.tokenizer.clone();
    if dtype.is_some() {
        loader_args.dtype = dtype;
    }

    if args.test.len() > 0 {
        run_tests(&args, loader_args);
        return;
    }

    let (tokenizer, tok_trie) =
        RllmEngine::load_tokenizer(&loader_args).expect("failed to load tokenizer");

    // make sure we try to load the model before spawning inference thread
    // otherwise, if the model doesn't exist, the inference thread will panic and things get messy
    let model_config =
        RllmEngine::load_model_config(&loader_args).expect("failed to load model config");

    let rt_args = rllm::iface::Args {
        aicirt: args.aicirt.clone(),
        tokenizer: args.tokenizer.clone(),
        json_size: args.json_size,
        bin_size: args.bin_size,
        shm_prefix: args.shm_prefix.clone(),
        busy_wait_time: args.busy_wait_time,
    };
    let stats = Arc::new(Mutex::new(ServerStats {
        num_requests: 0,
        num_tokens: 0,
        start_time: Instant::now(),
    }));
    let iface = AiciRtIface::start_aicirt(&rt_args, &tok_trie).expect("failed to start aicirt");
    let side_cmd_ch = iface.side_cmd.clone();
    let handle = spawn_inference_loop(&args, loader_args, iface, stats.clone());

    let app_data = OpenAIServerData {
        worker: handle.clone(),
        model_config,
        tokenizer: Arc::new(tokenizer),
        tok_trie: Arc::new(tok_trie),
        side_cmd_ch,
        stats,
    };
    let app_data = web::Data::new(app_data);
    let host = "127.0.0.1";

    println!("Listening at http://{}:{}", host, args.port);
    HttpServer::new(move || {
        App::new()
            .wrap(Logger::default())
            .service(models)
            .service(tunnel_info)
            .service(completion::completions)
            .service(get_aici_module_tags)
            .service(tag_aici_module)
            .configure(|cfg| {
                cfg.app_data(web::PayloadConfig::new(128 * 1024 * 1024))
                    .service(upload_aici_module);
            })
            .app_data(app_data.clone())
    })
    .workers(3)
    .bind((host, args.port))
    .expect("failed to start server (bind)")
    .run()
    .await
    .expect("failed to start server (run)");
}
