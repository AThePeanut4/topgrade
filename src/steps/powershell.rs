use std::path::PathBuf;
use std::process::Command;

#[cfg(windows)]
use color_eyre::eyre::eyre;
use color_eyre::eyre::Result;
use rust_i18n::t;
use tracing::debug;

use crate::command::CommandExt;
use crate::execution_context::ExecutionContext;
use crate::step::Step;
use crate::terminal;
use crate::utils::{which, PathExt};

pub struct Powershell {
    path: PathBuf,
    profile: Option<PathBuf>,
    is_pwsh: bool,
}

impl Powershell {
    pub fn new() -> Option<Self> {
        if terminal::is_dumb() {
            return None;
        }

        let (path, is_pwsh) = which("pwsh")
            .map(|p| (Some(p), true))
            .or_else(|| which("powershell").map(|p| (Some(p), false)))
            .unwrap_or((None, false));

        path.map(|path| {
            let mut ret = Self {
                path,
                profile: None,
                is_pwsh,
            };
            ret.set_profile();
            ret
        })
    }

    pub fn profile(&self) -> Option<&PathBuf> {
        self.profile.as_ref()
    }

    fn set_profile(&mut self) {
        let profile = self
            .build_command_internal("Split-Path $PROFILE")
            .output_checked_utf8()
            .map(|output| output.stdout.trim().to_string())
            .and_then(|s| PathBuf::from(s).require())
            .ok();
        debug!("Found PowerShell profile: {:?}", profile);
        self.profile = profile;
    }

    /// Builds an "internal" powershell command
    fn build_command_internal(&self, cmd: &str) -> Command {
        let mut command = Command::new(&self.path);

        command.args(["-NoProfile", "-Command"]);
        command.arg(cmd);

        // If topgrade was run from pwsh, but we are trying to run powershell, then
        // the inherited PSModulePath breaks module imports
        if !self.is_pwsh {
            command.env_remove("PSModulePath");
        }

        command
    }

    /// Builds a "primary" powershell command (uses dry-run if required):
    /// {powershell} -NoProfile -Command {cmd}
    fn build_command<'a>(&self, ctx: &'a ExecutionContext, cmd: &str, use_sudo: bool) -> Result<impl CommandExt + 'a> {
        let executor = &mut ctx.run_type();
        let mut command = if use_sudo && ctx.sudo().is_some() {
            let mut cmd = executor.execute(ctx.sudo().as_ref().unwrap());
            cmd.arg(&self.path);
            cmd
        } else {
            executor.execute(&self.path)
        };

        #[cfg(windows)]
        {
            // Check execution policy and return early if it's not set correctly
            self.execution_policy_args_if_needed()?;
        }

        command.args(["-NoProfile", "-Command"]);
        command.arg(cmd);

        // If topgrade was run from pwsh, but we are trying to run powershell, then
        // the inherited PSModulePath breaks module imports
        if !self.is_pwsh {
            command.env_remove("PSModulePath");
        }

        Ok(command)
    }

    pub fn update_modules(&self, ctx: &ExecutionContext) -> Result<()> {
        let mut cmd = "Update-Module".to_string();

        if ctx.config().verbose() {
            cmd.push_str(" -Verbose");
        }
        if ctx.config().yes(Step::Powershell) {
            cmd.push_str(" -Force");
        }

        println!("{}", t!("Updating modules..."));

        if self.is_pwsh {
            // For PowerShell Core, run Update-Module without sudo since it defaults to CurrentUser scope
            // and Update-Module updates all modules regardless of their original installation scope
            self.build_command(ctx, &cmd, false)?.status_checked()?;
        } else {
            // For (Windows) PowerShell, use sudo if available since it defaults to AllUsers scope
            // and may need administrator privileges
            self.build_command(ctx, &cmd, true)?.status_checked()?;
        }

        Ok(())
    }

    #[cfg(windows)]
    pub fn execution_policy_args_if_needed(&self) -> Result<()> {
        if !self.is_execution_policy_set("RemoteSigned") {
            Err(eyre!(
                "PowerShell execution policy is too restrictive. \
                Please run 'Set-ExecutionPolicy RemoteSigned -Scope CurrentUser' in PowerShell \
                (or use Unrestricted/Bypass if you're sure about the security implications)"
            ))
        } else {
            Ok(())
        }
    }

    #[cfg(windows)]
    fn is_execution_policy_set(&self, policy: &str) -> bool {
        // These policies are ordered from most restrictive to least restrictive
        let valid_policies = ["Restricted", "AllSigned", "RemoteSigned", "Unrestricted", "Bypass"];

        // Find the index of our target policy
        let target_idx = valid_policies.iter().position(|&p| p == policy);

        let current_policy = self
            .build_command_internal("Get-ExecutionPolicy")
            .output_checked_utf8()
            .map(|output| output.stdout.trim().to_string());

        debug!("Found PowerShell ExecutionPolicy: {:?}", current_policy);

        current_policy.is_ok_and(|current_policy| {
            // Find the index of the current policy
            let current_idx = valid_policies.iter().position(|&p| p == current_policy);

            // Check if current policy exists and is at least as permissive as the target
            match (current_idx, target_idx) {
                (Some(current), Some(target)) => current >= target,
                _ => false,
            }
        })
    }
}

#[cfg(windows)]
impl Powershell {
    fn has_module(&self, module_name: &str) -> bool {
        let cmd = format!("Get-Module -ListAvailable {}", module_name);

        self.build_command_internal(&cmd)
            .output_checked()
            .map(|output| !output.stdout.trim_ascii().is_empty())
            .unwrap_or(false)
    }

    pub fn supports_windows_update(&self) -> bool {
        self.has_module("PSWindowsUpdate")
    }

    pub fn windows_update(&self, ctx: &ExecutionContext) -> Result<()> {
        use crate::config::UpdatesAutoReboot;

        debug_assert!(self.supports_windows_update());

        let mut cmd = "Import-Module PSWindowsUpdate; Install-WindowsUpdate -Verbose".to_string();

        if ctx.config().accept_all_windows_updates() {
            cmd.push_str(" -AcceptAll");
        }

        match ctx.config().windows_updates_auto_reboot() {
            UpdatesAutoReboot::Yes => cmd.push_str(" -AutoReboot"),
            UpdatesAutoReboot::No => cmd.push_str(" -IgnoreReboot"),
            UpdatesAutoReboot::Ask => (), // Prompting is the default for Install-WindowsUpdate
        }

        self.build_command(ctx, &cmd, true)?.status_checked()
    }

    pub fn microsoft_store(&self, ctx: &ExecutionContext) -> Result<()> {
        println!("{}", t!("Scanning for updates..."));

        // Scan for updates using the MDM UpdateScanMethod
        // This method is also available for non-MDM devices
        let cmd = r#"(Get-CimInstance -Namespace "Root\cimv2\mdm\dmmap" -ClassName "MDM_EnterpriseModernAppManagement_AppManagement01" | Invoke-CimMethod -MethodName UpdateScanMethod).ReturnValue"#;

        self.build_command(ctx, cmd, true)?.output_checked_with_utf8(|output| {
            if !output.status.success() {
                return Err(());
            }
            let ret_val = output.stdout.trim();
            debug!("Command return value: {}", ret_val);
            if ret_val == "0" {
                Ok(())
            } else {
                Err(())
            }
        })?;
        println!(
            "{}",
            t!("Success, Microsoft Store apps are being updated in the background")
        );
        Ok(())
    }
}
