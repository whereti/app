//! The addresses whereti points to, the same as the website's src/links.ts
//! (whereti.website) and the installer's Windows "Apps" entry.

pub const SITE: &str = "https://whereti.com/";
pub const PRIVACY: &str = "https://whereti.com/privacy/";
pub const HELP: &str = "https://whereti.com/faq/";
pub const SOURCE: &str = "https://github.com/whereti/app";
pub const SUPPORTER: &str = "https://whereti.com/supporter/";
/// Personal goes straight to its Lemon Squeezy checkout. Business goes to the
/// website first: the checkout cannot change how many PCs, so the page asks.
pub const BUY_PERSONAL: &str = "https://whereti.lemonsqueezy.com/checkout/buy/ff4058f3-60ea-495b-8ea3-3a8a3f92d2ff";
pub const BUY_BUSINESS: &str = "https://whereti.com/supporter/#business";
