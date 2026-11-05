//@ edition: 2021
pub fn unset() -> bool { option_env!("BLAZE_CASES_SURELY_UNSET_VARIABLE").is_none() }
pub fn label() -> &'static str { match option_env!("BLAZE_CASES_SURELY_UNSET_VARIABLE") { Some(v) => v, None => "unset" } }
