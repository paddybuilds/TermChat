use std::sync::Once;

static INSTALL_PROVIDER: Once = Once::new();

pub(crate) fn install_crypto_provider() {
    INSTALL_PROVIDER.call_once(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}
