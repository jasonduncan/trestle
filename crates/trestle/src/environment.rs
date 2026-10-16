use clap::ValueEnum;

/// Where the process runs (`TRESTLE_ENV`). Production refuses unsafe settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Environment {
    Development,
    Test,
    Production,
}

impl Environment {
    pub fn is_production(self) -> bool {
        self == Environment::Production
    }
}
