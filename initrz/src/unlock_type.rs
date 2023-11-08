pub enum UnlockType {
    AskPassphrase,
    Key(String),
}

impl From<&str> for UnlockType {
    fn from(unlock_type: &str) -> UnlockType {
        match unlock_type {
            "none" => UnlockType::AskPassphrase,
            _ => UnlockType::Key(unlock_type.into()),
        }
    }
}
