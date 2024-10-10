#![deny(warnings)]

use {
    exports::wasi::http::incoming_handler::Guest,
    std::{
        collections::BTreeMap,
        str::{self, FromStr},
    },
    wasi::{
        filesystem::{
            preopens,
            types::{DescriptorFlags, DirectoryEntry, OpenFlags, PathFlags},
        },
        http::types::{
            Fields, IncomingBody, IncomingRequest, OutgoingBody, OutgoingResponse, ResponseOutparam,
        },
    },
};

wit_bindgen::generate!({
    world: "proxy-etcetera",
    path: "../wit",
    features: ["http-body-append"],
    generate_all,
});

struct Component;

export!(Component);

impl Guest for Component {
    fn handle(request: IncomingRequest, response_out: ResponseOutparam) {
        let use_append = match request.path_with_query().as_deref() {
            Some("/stream") => false,
            Some("/append") => true,
            value => panic!("unsupported URI: {value:?}"),
        };
        let request_length = u64::from_str(
            str::from_utf8(
                &request
                    .headers()
                    .get(&"content-length".to_string())
                    .into_iter()
                    .next()
                    .unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
        let request_body = request.consume().unwrap();
        let request_stream = request_body.stream().unwrap();
        let response = OutgoingResponse::new(Fields::new());
        let response_body = response.body().unwrap();
        let response_stream = response_body.write().unwrap();

        ResponseOutparam::set(response_out, Ok(response));

        if use_append {
            response_body
                .append(request_stream, Some(request_length))
                .unwrap();
        } else {
            let mut remaining = request_length;
            while remaining > 0 {
                remaining -= response_stream
                    .blocking_splice(&request_stream, remaining)
                    .unwrap();
            }

            drop(request_stream);
        }

        IncomingBody::finish(request_body);

        let (directory, _) = preopens::get_directories().into_iter().next().unwrap();
        let directory_stream = directory.read_directory().unwrap();
        let mut files = BTreeMap::new();
        while let Some(DirectoryEntry { name, .. }) =
            directory_stream.read_directory_entry().unwrap()
        {
            let file = directory
                .open_at(
                    PathFlags::empty(),
                    &name,
                    OpenFlags::empty(),
                    DescriptorFlags::READ,
                )
                .unwrap();
            files.insert(name, file);
        }

        for file in files.values() {
            let file_stream = file.read_via_stream(0).unwrap();
            let file_length = file.stat().unwrap().size;
            if use_append {
                response_body
                    .append(file_stream, Some(file_length))
                    .unwrap();
            } else {
                let mut remaining = file_length;
                while remaining > 0 {
                    remaining -= response_stream
                        .blocking_splice(&file_stream, remaining)
                        .unwrap();
                }
            }
        }

        OutgoingBody::finish(response_body, None).unwrap();
    }
}
