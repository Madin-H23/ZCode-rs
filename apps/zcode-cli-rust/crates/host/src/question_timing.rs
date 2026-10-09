use anyhow::{Context, Result};

/// AskUserQuestion `(hidden, deadline)` in ms; the E2E clock scale is read
/// from the runtime's environment snapshot, never the process environment.
pub fn question_timing(env: &[(String, String)]) -> Result<(u64, u64)> {
    let mut scale = 1.0;
    if crate::env_var(env, "ZCODE_ENV") == Some("test")
        && let Some(raw) = crate::env_var(env, "ZCODE_E2E_ASK_USER_QUESTION_CLOCK_SCALE")
        && !raw.trim().is_empty()
    {
        scale = raw
            .trim()
            .parse::<f64>()
            .context("Invalid AskUserQuestion clock scale")?;
        anyhow::ensure!(
            scale.is_finite() && (1.0..=1000.0).contains(&scale),
            "AskUserQuestion clock scale must be between 1 and 1000"
        );
    }
    Ok((
        (60_000.0 / scale).round().max(1.0) as u64,
        (300_000.0 / scale).round().max(1.0) as u64,
    ))
}

#[cfg(test)]
mod tests {
    use super::question_timing;

    #[test]
    fn the_clock_scale_comes_from_the_snapshot() {
        let env = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect()
        };
        assert_eq!(question_timing(&[]).unwrap(), (60_000, 300_000));
        let scaled = env(&[
            ("ZCODE_ENV", "test"),
            ("ZCODE_E2E_ASK_USER_QUESTION_CLOCK_SCALE", " 100 "),
        ]);
        assert_eq!(question_timing(&scaled).unwrap(), (600, 3_000));
        let production = env(&[("ZCODE_E2E_ASK_USER_QUESTION_CLOCK_SCALE", "100")]);
        assert_eq!(question_timing(&production).unwrap(), (60_000, 300_000));
        let invalid = env(&[
            ("ZCODE_ENV", "test"),
            ("ZCODE_E2E_ASK_USER_QUESTION_CLOCK_SCALE", "0.5"),
        ]);
        assert!(question_timing(&invalid).is_err());
    }
}
