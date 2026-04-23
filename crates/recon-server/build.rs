fn main() {
    // In release builds the HMAC key must be set explicitly.
    // Without it, the binary uses the public dev placeholder and any user
    // can forge a valid license by reading the source code.
    #[cfg(not(debug_assertions))]
    if std::env::var("RECON_LICENSE_HMAC_KEY").is_err() {
        panic!(
            "\n\nRECON_LICENSE_HMAC_KEY is not set.\n\
             Release binaries must be built with a secret HMAC key:\n\
             RECON_LICENSE_HMAC_KEY=<secret> cargo build --release\n\
             Set it as a GitHub Actions secret and pass it in release.yml.\n"
        );
    }

    println!("cargo:rerun-if-env-changed=RECON_LICENSE_HMAC_KEY");
}
