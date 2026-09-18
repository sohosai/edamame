//! 外部配信への振り分け

use std::net::IpAddr;

/// 公開範囲の遅れから外部へ振り分ける割合（%）を求める
///
/// - `published_delay`：公開範囲が最新セグメントより遅れているセグメント数
/// - `normal_delay`：公開待ちで生じる通常の遅れ
/// - `full_delay`：全量を外部へ流す遅れ
/// - `min_percent`：外部へ流す最低の割合（%）
///
/// 遅れが `normal_delay` 以下なら `min_percent` を返し、`full_delay` 以上なら 100 を返す。
/// その間は遅れに比例して上げる。
pub fn external_routing_percent(
    published_delay: u64,
    normal_delay: u64,
    full_delay: u64,
    min_percent: u8,
) -> u8 {
    let min_percent = min_percent.min(100);
    // 公開待ちの時間が長い設定では normal_delay が full_delay 以上になり得るため、先に判定する
    if published_delay >= full_delay {
        return 100;
    }
    if published_delay <= normal_delay {
        return min_percent;
    }
    let span = full_delay.saturating_sub(normal_delay).max(1);
    let progress = (published_delay - normal_delay).min(span);
    let min = u64::from(min_percent);
    let percent = min + (100 - min) * progress / span;
    u8::try_from(percent.min(100)).unwrap_or(100)
}

/// 接続元ごとに固定した抽選で、外部配信へ流すかどうかを返す
///
/// 同じ channel と接続元 IP の組み合わせは、割合が変わらない限り同じ結果になる。
pub fn routes_to_external(channel_id: &str, client_ip: IpAddr, external_percent: u8) -> bool {
    if external_percent == 0 {
        return false;
    }
    // 実行をまたいでも同じ接続元が同じ結果になるよう、channel と接続元から値を決める
    let client = client_ip.to_string();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in channel_id
        .as_bytes()
        .iter()
        .chain(b"\n")
        .chain(client.as_bytes())
    {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash % 100 < u64::from(external_percent.min(100))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(value: &str) -> IpAddr {
        value.parse().unwrap()
    }

    #[test]
    fn keeps_minimum_percent_until_normal_delay() {
        assert_eq!(external_routing_percent(0, 1, 6, 1), 1);
        assert_eq!(external_routing_percent(1, 1, 6, 1), 1);
    }

    #[test]
    fn ramps_percent_toward_full_delay() {
        // normal 1 から full 6 までを線形に上げる
        assert_eq!(external_routing_percent(2, 1, 6, 1), 20);
        assert_eq!(external_routing_percent(4, 1, 6, 1), 60);
        assert_eq!(external_routing_percent(6, 1, 6, 1), 100);
        assert_eq!(external_routing_percent(10, 1, 6, 1), 100);
    }

    #[test]
    fn keeps_minimum_percent_when_disabled() {
        assert_eq!(external_routing_percent(0, 0, 6, 0), 0);
        assert_eq!(external_routing_percent(1, 0, 6, 0), 16);
    }

    #[test]
    fn stays_full_when_minimum_percent_is_full() {
        assert_eq!(external_routing_percent(2, 1, 6, 100), 100);
    }

    #[test]
    fn reaches_full_percent_when_normal_delay_is_longer() {
        // 公開待ちが長い設定では normal_delay が full_delay を超えるため、full を先に判定する
        assert_eq!(external_routing_percent(6, 10, 6, 1), 100);
        assert_eq!(external_routing_percent(7, 10, 6, 1), 100);
    }

    #[test]
    fn routes_no_client_when_percent_is_zero() {
        for value in 0..50 {
            let address = ip(&format!("192.0.2.{value}"));
            assert!(!routes_to_external("live", address, 0));
        }
    }

    #[test]
    fn routes_every_client_when_percent_is_full() {
        for value in 0..50 {
            let address = ip(&format!("192.0.2.{value}"));
            assert!(routes_to_external("live", address, 100));
        }
    }

    #[test]
    fn selects_about_one_percent_of_clients() {
        let mut selected = 0;
        for value in 0..10_000 {
            let address = ip(&format!("198.51.{}.{}", value / 256, value % 256));
            if routes_to_external("live", address, 1) {
                selected += 1;
            }
        }
        assert!(
            (50..=200).contains(&selected),
            "selected {selected} of 10000"
        );
    }

    #[test]
    fn keeps_selection_stable_and_monotonic() {
        let address = ip("203.0.113.7");
        let selected = routes_to_external("live", address, 30);
        assert_eq!(selected, routes_to_external("live", address, 30));
        if selected {
            assert!(routes_to_external("live", address, 60));
        }
        // channel が違えば選ばれる接続元も変わる
        let _ = routes_to_external("other", address, 30);
    }
}
