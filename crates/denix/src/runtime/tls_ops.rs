use deno_core::extension;
use deno_runtime::deno_node::ops::tls::op_tls_peer_certificate;

extension!(denix_tls_ops, ops = [op_tls_peer_certificate]);

pub fn init_tls_ops_extension() -> deno_core::Extension {
    denix_tls_ops::init()
}
