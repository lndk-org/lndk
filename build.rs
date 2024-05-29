fn main() -> Result<(), Box<dyn std::error::Error>> {
    configure_me_codegen::build_script_auto().unwrap_or_else(|error| error.report_and_exit());

    // Compile the protos for our grpc server.
    tonic_build::configure()
        .protoc_arg("--experimental_allow_proto3_optional")
        .compile(&["proto/lndkrpc.proto"], &["proto"])?;

    Ok(())
}
