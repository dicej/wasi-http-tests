#![deny(warnings)]
#![feature(test)]

extern crate test;

#[cfg(test)]
mod tests {
    use {
        anyhow::{anyhow, bail, Result},
        futures::future,
        http_body_util::{BodyExt, Full},
        hyper::{body::Bytes, Request, Response},
        rand::{rngs::StdRng, Rng, SeedableRng},
        std::{cell::RefCell, future::Future, hint, iter, rc::Rc, sync::Once},
        tempfile::TempDir,
        test::Bencher,
        tokio::{
            fs,
            process::Command,
            runtime::Runtime,
            sync::oneshot,
            sync::OnceCell,
            task::{self, JoinHandle},
        },
        wasmtime::{
            component::{Component, Linker, ResourceTable},
            Config, Engine, InstanceAllocationStrategy, PoolingAllocationConfig, Store,
        },
        wasmtime_wasi::{DirPerms, FilePerms, WasiCtx, WasiCtxBuilder, WasiView},
        wasmtime_wasi_http::{
            bindings::{http::types::Scheme, LinkOptions, Proxy, ProxyPre},
            body::HyperOutgoingBody,
            WasiHttpCtx, WasiHttpView,
        },
    };

    struct Context {
        table: ResourceTable,
        wasi: WasiCtx,
        wasi_http: WasiHttpCtx,
    }

    impl WasiView for Context {
        fn table(&mut self) -> &mut ResourceTable {
            &mut self.table
        }
        fn ctx(&mut self) -> &mut WasiCtx {
            &mut self.wasi
        }
    }

    impl WasiHttpView for Context {
        fn table(&mut self) -> &mut ResourceTable {
            &mut self.table
        }
        fn ctx(&mut self) -> &mut WasiHttpCtx {
            &mut self.wasi_http
        }
    }

    async fn build_component(name: &str) -> Result<Vec<u8>> {
        if Command::new("cargo")
            .current_dir(format!("../{name}"))
            .args(["+nightly", "build", "--target", "wasm32-wasip2"])
            .status()
            .await?
            .success()
        {
            Ok(fs::read(format!("../target/wasm32-wasip2/debug/{name}.wasm")).await?)
        } else {
            Err(anyhow!("cargo build failed"))
        }
    }

    struct Content {
        first_part: &'static str,
        content: String,
        directory: TempDir,
    }

    async fn make_content() -> Result<Content> {
        let first_part = "'Twas brillig, and the slithy toves\
                          Did gyre and gimble in the wabe:\
                          All mimsy were the borogoves,\
                          And the mome raths outgrabe.\n\n";

        let directory = tempfile::tempdir()?;
        let mut rng = StdRng::seed_from_u64(42);
        let file_count = 4;
        let file_size = 1024 * 1024;
        let mut content = first_part.to_string();
        for i in 0..file_count {
            let file_content = iter::repeat_with(|| rng.gen_range('a'..='z'))
                .take(file_size)
                .collect::<String>();
            fs::write(
                directory.path().join(format!("{i}")),
                file_content.as_bytes(),
            )
            .await?;
            content.push_str(&file_content);
        }

        Ok(Content {
            first_part,
            content,
            directory,
        })
    }

    struct State {
        engine: Engine,
        proxy_pre: ProxyPre<Context>,
        content: Content,
    }

    async fn make_state() -> Result<State> {
        let mut config = Config::new();
        config.async_support(true);
        config.wasm_component_model(true);
        config.allocation_strategy(InstanceAllocationStrategy::Pooling(
            PoolingAllocationConfig::default(),
        ));

        let engine = Engine::new(&config)?;

        let mut linker = Linker::new(&engine);

        wasmtime_wasi::add_to_linker_async(&mut linker)?;
        wasmtime_wasi_http::add_only_http_to_linker_with_options_async(&mut linker, &{
            let mut options = LinkOptions::default();
            options.http_body_append(true);
            options
        })?;

        let component = Component::new(&engine, build_component("guest").await?)?;

        let proxy_pre = ProxyPre::new(linker.instantiate_pre(&component)?)?;

        let content = make_content().await?;

        Ok(State {
            engine,
            proxy_pre,
            content,
        })
    }

    async fn state() -> &'static State {
        static INIT_LOGGER: Once = Once::new();
        INIT_LOGGER.call_once(pretty_env_logger::init);

        static STATE: OnceCell<State> = OnceCell::const_new();
        STATE
            .get_or_init(|| async { make_state().await.unwrap() })
            .await
    }

    fn make_context(state: &State) -> Result<Context> {
        Ok(Context {
            table: ResourceTable::new(),
            wasi: WasiCtxBuilder::new()
                .inherit_stdio()
                .preopened_dir(
                    state.content.directory.path(),
                    ".",
                    DirPerms::READ,
                    FilePerms::READ,
                )?
                .build(),
            wasi_http: WasiHttpCtx::new(),
        })
    }

    fn make_store(state: &State, context: Context) -> Result<Store<Context>> {
        Ok(Store::new(&state.engine, context))
    }

    #[derive(Copy, Clone)]
    enum Strategy {
        Stream,
        Append,
        StreamThenAppend,
    }

    impl Strategy {
        fn uri(&self) -> &'static str {
            match self {
                Self::Stream => "http://localhost/stream",
                Self::Append => "http://localhost/append",
                Self::StreamThenAppend => "http://localhost/stream-then-append",
            }
        }
    }

    async fn run(
        state: &State,
        context: Context,
        strategy: Strategy,
    ) -> Result<(
        JoinHandle<Result<(Store<Context>, Proxy)>>,
        Response<HyperOutgoingBody>,
    )> {
        let mut store = make_store(state, context)?;
        let proxy = state.proxy_pre.instantiate_async(&mut store).await?;
        run_instance(state, store, proxy, strategy).await
    }

    async fn run_instance(
        state: &State,
        mut store: Store<Context>,
        proxy: Proxy,
        strategy: Strategy,
    ) -> Result<(
        JoinHandle<Result<(Store<Context>, Proxy)>>,
        Response<HyperOutgoingBody>,
    )> {
        let request = store.data_mut().new_incoming_request(
            Scheme::Http,
            Request::builder()
                .uri(strategy.uri())
                .header("content-length", state.content.first_part.len().to_string())
                .body(
                    Full::new(Bytes::from_static(state.content.first_part.as_bytes()))
                        .map_err(|_| unreachable!()),
                )?,
        )?;
        let (tx, rx) = oneshot::channel();

        let response_out = store.data_mut().new_response_outparam(tx)?;

        let task = task::spawn(async move {
            proxy
                .wasi_http_incoming_handler()
                .call_handle(&mut store, request, response_out)
                .await?;

            Ok((store, proxy))
        });

        match rx.await {
            Ok(Ok(response)) => Ok((task, response)),
            Ok(Err(e)) => Err(anyhow::Error::from(e)),
            Err(_) => {
                let e = match task.await {
                    Ok(r) => r
                        .map(drop)
                        .expect_err("if the receiver has an error, the task must have failed"),
                    Err(e) => anyhow::Error::from(e),
                };
                bail!("guest never invoked `response-outparam::set` method: {e:?}")
            }
        }
    }

    async fn test(strategy: Strategy) -> Result<()> {
        let state = state().await;
        let (_, response) = run(state, make_context(state)?, strategy).await?;
        assert!(
            state.content.content.as_bytes() == read(response, strategy, future::ok(())).await?
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stream() -> Result<()> {
        test(Strategy::Stream).await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn append() -> Result<()> {
        test(Strategy::Append).await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stream_then_append() -> Result<()> {
        test(Strategy::StreamThenAppend).await
    }

    async fn read(
        response: Response<HyperOutgoingBody>,
        strategy: Strategy,
        recycle: impl Future<Output = Result<()>>,
    ) -> Result<Bytes> {
        let mut recycle = Some(recycle);

        if let Strategy::Append = strategy {
            recycle.take().unwrap().await?;
        }

        let bytes = response.into_body().collect().await?.to_bytes();

        if let Strategy::Stream | Strategy::StreamThenAppend = strategy {
            recycle.take().unwrap().await?;
        }

        Ok(bytes)
    }

    fn run_without_reuse(bencher: &mut Bencher, strategy: Strategy) {
        let runtime = Runtime::new().unwrap();

        let state = runtime.block_on(state());
        let context = Rc::new(RefCell::new(Some(make_context(state).unwrap())));

        let run = move || {
            let context = context.clone();
            async move {
                let old_context = context.borrow_mut().take().unwrap();

                let (task, response) = run(state, old_context, strategy).await?;

                hint::black_box(
                    read(response, strategy, async move {
                        let mut ctx = task.await??.0.into_data();
                        ctx.table = ResourceTable::new();
                        *context.borrow_mut() = Some(ctx);
                        Ok(())
                    })
                    .await?,
                );

                Ok::<_, anyhow::Error>(())
            }
        };

        bencher.iter(|| runtime.block_on(run()).unwrap());
    }

    fn run_with_reuse(bencher: &mut Bencher, strategy: Strategy) {
        let runtime = Runtime::new().unwrap();

        let state = runtime.block_on(state());

        let mut store = make_store(state, make_context(state).unwrap()).unwrap();
        let proxy = Rc::new(RefCell::new(Some(
            runtime
                .block_on(state.proxy_pre.instantiate_async(&mut store))
                .unwrap(),
        )));
        let store = Rc::new(RefCell::new(Some(store)));

        let run = move || {
            let store = store.clone();
            let proxy = proxy.clone();
            async move {
                let old_store = store.borrow_mut().take().unwrap();
                let old_proxy = proxy.borrow_mut().take().unwrap();

                let (task, response) = run_instance(state, old_store, old_proxy, strategy).await?;

                hint::black_box(
                    read(response, strategy, async move {
                        let (new_store, new_proxy) = task.await??;
                        *store.borrow_mut() = Some(new_store);
                        *proxy.borrow_mut() = Some(new_proxy);
                        Ok(())
                    })
                    .await?,
                );

                Ok::<_, anyhow::Error>(())
            }
        };

        bencher.iter(|| runtime.block_on(run()).unwrap());
    }

    #[bench]
    fn stream_without_reuse(bencher: &mut Bencher) {
        run_without_reuse(bencher, Strategy::Stream)
    }

    #[bench]
    fn stream_with_reuse(bencher: &mut Bencher) {
        run_with_reuse(bencher, Strategy::Stream)
    }

    #[bench]
    fn append_without_reuse(bencher: &mut Bencher) {
        run_without_reuse(bencher, Strategy::Append)
    }

    #[bench]
    fn append_with_reuse(bencher: &mut Bencher) {
        run_with_reuse(bencher, Strategy::Append)
    }
}
