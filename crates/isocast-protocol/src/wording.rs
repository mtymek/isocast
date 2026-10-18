//! Words that name the computer and its parts in messages, so each
//! system's people read their own: "this Mac" and "the Keychain" on macOS,
//! "this computer" and "the keyring" on Linux.

/// The computer Isocast runs on.
pub const THIS_COMPUTER: &str = if cfg!(target_os = "macos") {
    "this Mac"
} else if cfg!(windows) {
    "this PC"
} else {
    "this computer"
};

/// [`THIS_COMPUTER`]'s, possessive.
pub const THIS_COMPUTERS: &str = if cfg!(target_os = "macos") {
    "this Mac's"
} else if cfg!(windows) {
    "this PC's"
} else {
    "this computer's"
};

/// Where secrets such as stream keys are kept.
pub const KEYRING: &str = if cfg!(target_os = "macos") {
    "the Keychain"
} else if cfg!(windows) {
    "Credential Manager"
} else {
    "the keyring"
};

/// Who asks for a permission the first time it is needed.
pub const ASKS: &str = if cfg!(target_os = "macos") {
    "macOS asks"
} else if cfg!(windows) {
    "Windows asks"
} else {
    "the desktop asks"
};
