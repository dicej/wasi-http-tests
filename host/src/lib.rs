#![deny(warnings)]

#[cfg(test)]
mod tests {
    use {
        anyhow::{anyhow, bail, Result},
        http_body_util::{BodyExt, Full},
        hyper::{body::Bytes, Request},
        rand::{rngs::StdRng, Rng, SeedableRng},
        std::{iter, sync::Once},
        tokio::{fs, process::Command, sync::oneshot, sync::OnceCell, task},
        wasmtime::{
            component::{Component, Linker, ResourceTable},
            Config, Engine, Store,
        },
        wasmtime_wasi::{DirPerms, FilePerms, WasiCtx, WasiCtxBuilder, WasiView},
        wasmtime_wasi_http::{
            bindings::{http::types::Scheme, LinkOptions, Proxy},
            WasiHttpCtx, WasiHttpView,
        },
    };

    struct Ctx {
        table: ResourceTable,
        wasi: WasiCtx,
        wasi_http: WasiHttpCtx,
    }

    impl WasiView for Ctx {
        fn table(&mut self) -> &mut ResourceTable {
            &mut self.table
        }
        fn ctx(&mut self) -> &mut WasiCtx {
            &mut self.wasi
        }
    }

    impl WasiHttpView for Ctx {
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

    async fn test(component: &[u8], uri: &str) -> Result<()> {
        static ONCE: Once = Once::new();
        ONCE.call_once(pretty_env_logger::init);

        let mut config = Config::new();
        config.wasm_component_model(true);
        config.async_support(true);

        let engine = Engine::new(&config)?;

        let component = Component::new(&engine, component)?;

        let mut linker = Linker::new(&engine);

        wasmtime_wasi::add_to_linker_async(&mut linker)?;
        wasmtime_wasi_http::add_only_http_to_linker_with_options_async(&mut linker, &{
            let mut options = LinkOptions::default();
            options.http_body_append(true);
            options
        })?;

        let first_part = "'Twas brillig, and the slithy toves\
                          Did gyre and gimble in the wabe:\
                          All mimsy were the borogoves,\
                          And the mome raths outgrabe.\n\n";

        let temp_dir = tempfile::tempdir()?;
        let mut rng = StdRng::seed_from_u64(42);
        let file_count = 4;
        let file_size = 1024 * 1024;
        let mut content = first_part.to_string();
        for i in 0..file_count {
            let file_content = iter::repeat_with(|| rng.gen_range('a'..='z'))
                .take(file_size)
                .collect::<String>();
            fs::write(
                temp_dir.path().join(format!("{i}")),
                file_content.as_bytes(),
            )
            .await?;
            content.push_str(&file_content);
        }

        let mut store = Store::new(
            &engine,
            Ctx {
                table: ResourceTable::new(),
                wasi: WasiCtxBuilder::new()
                    .inherit_stdio()
                    .preopened_dir(temp_dir.path(), ".", DirPerms::READ, FilePerms::READ)?
                    .build(),
                wasi_http: WasiHttpCtx::new(),
            },
        );

        let request = store.data_mut().new_incoming_request(
            Scheme::Http,
            Request::builder()
                .uri(format!("http://localhost{uri}"))
                .header("content-length", first_part.len().to_string())
                .body(
                    Full::new(Bytes::from_static(first_part.as_bytes()))
                        .map_err(|_| unreachable!()),
                )?,
        )?;
        let (tx, rx) = oneshot::channel();

        let response_out = store.data_mut().new_response_outparam(tx)?;
        let proxy = Proxy::instantiate_async(&mut store, &component, &linker).await?;

        let task = task::spawn(async move {
            proxy
                .wasi_http_incoming_handler()
                .call_handle(&mut store, request, response_out)
                .await?;

            Ok(())
        });

        let response = match rx.await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(e)) => Err(anyhow::Error::from(e)),
            Err(_) => {
                let e = match task.await {
                    Ok(r) => {
                        r.expect_err("if the receiver has an error, the task must have failed")
                    }
                    Err(e) => anyhow::Error::from(e),
                };
                bail!("guest never invoked `response-outparam::set` method: {e:?}")
            }
        }?;

        assert!(content.as_bytes() == response.into_body().collect().await?.to_bytes());

        Ok(())
    }

    async fn guest() -> &'static [u8] {
        static ONCE: OnceCell<Vec<u8>> = OnceCell::const_new();
        &ONCE
            .get_or_init(|| async { build_component("guest").await.unwrap() })
            .await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stream() -> Result<()> {
        test(guest().await, "/stream").await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn append() -> Result<()> {
        test(guest().await, "/append").await
    }
}
