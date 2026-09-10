//! Wall-clock formatting for the session list: local `YYYY-MM-DD HH:MM` from unix seconds,
//! and durations as people say them. libc's `localtime_r` keeps this dependency-free.

pub fn local_datetime(secs: i64) -> String {
    let (y, mo, d, h, mi) = local_parts(secs);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}")
}

pub fn local_time(secs: i64) -> String {
    let (_, _, _, h, mi) = local_parts(secs);
    format!("{h:02}:{mi:02}")
}

fn local_parts(secs: i64) -> (i32, u32, u32, u32, u32) {
    let t: libc::time_t = secs as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are valid for the call; localtime_r writes only into `tm`.
    let ok = unsafe { !libc::localtime_r(&t, &mut tm).is_null() };
    if !ok {
        return (1970, 1, 1, 0, 0);
    }
    (
        tm.tm_year + 1900,
        (tm.tm_mon + 1) as u32,
        tm.tm_mday as u32,
        tm.tm_hour as u32,
        tm.tm_min as u32,
    )
}

/// `42m`, `1h 05m`, `30s`.
pub fn human_duration(secs: i64) -> String {
    let s = secs.max(0);
    if s >= 3600 {
        format!("{}h {:02}m", s / 3600, (s % 3600) / 60)
    } else if s >= 60 {
        format!("{}m", s / 60)
    } else {
        format!("{s}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_look_like_dates_and_durations() {
        let s = local_datetime(1_756_000_000);
        assert_eq!(s.len(), 16, "{s}");
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[10..11], " ");
        assert_eq!(local_time(1_756_000_000), &s[11..]);
        assert_eq!(human_duration(30), "30s");
        assert_eq!(human_duration(2520), "42m");
        assert_eq!(human_duration(3900), "1h 05m");
        assert_eq!(human_duration(-5), "0s");
    }
}
