//! tomyou のライブラリ部分
//!
//! GUI の起動はバイナリ側で行い、RTMP 中継や設定をテストから利用できるようにする。

pub mod app;
pub mod config;
pub mod credentials;
pub mod gstreamer;
pub mod imageflux;
pub mod logging;
pub mod obs;
pub mod relay;
pub mod session;
pub mod state;
pub mod stream_info;

/// アプリのバージョン
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// タイトルバーに出す名前
pub fn window_title() -> String {
    format!("tomyou v{VERSION}")
}

#[cfg(test)]
mod tests {
    #[test]
    fn window_title_contains_version() {
        let title = super::window_title();
        assert!(title.contains(super::VERSION));
        assert!(title.starts_with("tomyou v"));
    }
}
