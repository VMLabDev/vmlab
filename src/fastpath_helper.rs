//! `vmlab-fastpath`, the eBPF fast path's privileged helper (F21). The
//! only vmlab binary `install.sh` grants CAP_BPF and CAP_NET_ADMIN; see
//! `net::fastpath::helper` in the library.

fn main() -> std::process::ExitCode {
    vmlab::fastpath_helper_main()
}
