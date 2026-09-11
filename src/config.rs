use std::{
    env,
    error::Error,
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
};

const DEFAULT_COLOR: u32 = 0x4B3F72;
const DEFAULT_OFF_AFTER: u64 = 600;

pub(crate) struct Config {
    pub(crate) color: u32,
    pub(crate) off_after: u64,
    pub(crate) power_off_command: Option<Vec<String>>,
    pub(crate) pam_service: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            color: DEFAULT_COLOR,
            off_after: DEFAULT_OFF_AFTER,
            power_off_command: None,
            pam_service: String::from("swaylock"),
        }
    }
}

pub(crate) fn parse_args() -> Result<Config, Box<dyn Error>> {
    let args: Vec<String> = env::args().skip(1).collect();
    let mut config = Config::default();
    let mut config_path = default_config_path();
    let mut load_config_file = true;

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--config" => {
                index += 1;
                let value = args.get(index).ok_or("--config requires a path")?;
                config_path = Some(PathBuf::from(value));
            }
            "--no-config" => load_config_file = false,
            _ => {}
        }
        index += 1;
    }

    if load_config_file {
        if let Some(path) = config_path {
            load_config_file_from(&path, &mut config)?;
        }
    }

    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("usage: waylock-rs [--config PATH] [--no-config] [OPTIONS]");
                println!("\nconfig file: ~/.config/waylock-rs/config");
                println!("options override values from the config file:");
                println!("  --color RRGGBB");
                println!("  --off-after SECONDS");
                println!("  --power-off-command PROGRAM [ARGS...]");
                println!("  --pam-service NAME");
                std::process::exit(0);
            }
            "--version" | "-V" => {
                println!("waylock-rs 0.1.0");
                std::process::exit(0);
            }
            "--color" => {
                let value = args.next().ok_or("--color requires RRGGBB")?;
                config.color = parse_color(&value)?;
            }
            "--off-after" => {
                config.off_after = args.next().ok_or("--off-after requires seconds")?.parse()?;
            }
            "--power-off-command" => {
                let program = args
                    .next()
                    .ok_or("--power-off-command requires a program")?;
                let mut command = vec![program];
                command.extend(args);
                config.power_off_command = Some(command);
                break;
            }
            "--pam-service" => {
                config.pam_service = args.next().ok_or("--pam-service requires a name")?;
            }
            "--config" => {
                let _ = args.next().ok_or("--config requires a path")?;
            }
            "--no-config" => {}
            other => return Err(format!("unknown argument: {other}").into()),
        }
    }

    Ok(config)
}

fn default_config_path() -> Option<PathBuf> {
    if let Some(path) = env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(path).join("waylock-rs/config"));
    }
    env::var_os("HOME").map(|home| PathBuf::from(home).join(".config/waylock-rs/config"))
}

fn load_config_file_from(path: &Path, config: &mut Config) -> Result<(), Box<dyn Error>> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("cannot read {}: {error}", path.display()).into()),
    };

    for (line_number, line) in contents.lines().enumerate() {
        let line = line.split('#').next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!(
                "{}:{}: expected key = value",
                path.display(),
                line_number + 1
            )
            .into());
        };
        apply_config_value(
            config,
            key.trim(),
            unquote(value.trim()),
            &format!("{}:{}", path.display(), line_number + 1),
        )?;
    }
    Ok(())
}

fn apply_config_value(
    config: &mut Config,
    key: &str,
    value: &str,
    source: &str,
) -> Result<(), Box<dyn Error>> {
    match key {
        "color" => config.color = parse_color(value)?,
        "off-after" | "off_after" => {
            config.off_after = value
                .parse()
                .map_err(|_| format!("{source}: invalid off-after"))?;
        }
        "power-off-command" | "power_off_command" => {
            config.power_off_command = if value.trim().is_empty() {
                None
            } else {
                Some(value.split_whitespace().map(String::from).collect())
            };
        }
        "pam-service" | "pam_service" => config.pam_service = value.to_string(),
        other => return Err(format!("{source}: unknown key {other}").into()),
    }
    Ok(())
}

fn unquote(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .or_else(|| {
            value
                .strip_prefix('\'')
                .and_then(|value| value.strip_suffix('\''))
        })
        .unwrap_or(value)
}

fn parse_color(value: &str) -> Result<u32, Box<dyn Error>> {
    let value = value.strip_prefix('#').unwrap_or(value);
    if value.len() != 6 {
        return Err("color must be exactly six hexadecimal digits".into());
    }
    Ok(u32::from_str_radix(value, 16)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_values_are_applied() {
        let mut config = Config::default();
        apply_config_value(&mut config, "color", "123456", "test").unwrap();
        apply_config_value(&mut config, "off-after", "42", "test").unwrap();
        apply_config_value(
            &mut config,
            "power-off-command",
            "niri msg action power-off-monitors",
            "test",
        )
        .unwrap();
        apply_config_value(&mut config, "pam-service", "login", "test").unwrap();

        assert_eq!(config.color, 0x123456);
        assert_eq!(config.off_after, 42);
        assert_eq!(
            config.power_off_command,
            Some(vec![
                String::from("niri"),
                String::from("msg"),
                String::from("action"),
                String::from("power-off-monitors"),
            ])
        );
        assert_eq!(config.pam_service, "login");
    }

    #[test]
    fn empty_power_off_command_disables_action() {
        let mut config = Config::default();
        apply_config_value(&mut config, "power-off-command", "", "test").unwrap();
        assert!(config.power_off_command.is_none());
    }

    #[test]
    fn whitespace_power_off_command_disables_action() {
        let mut config = Config::default();
        apply_config_value(&mut config, "power-off-command", "   ", "test").unwrap();
        assert!(config.power_off_command.is_none());
    }
}
