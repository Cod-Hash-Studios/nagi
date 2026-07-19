use crate::doctor::{CheckStatus, DoctorReport};

pub(super) fn run_doctor_command(args: &[String]) -> std::io::Result<i32> {
    let json = match args {
        [] => false,
        [flag] if flag == "--json" => true,
        [flag] if matches!(flag.as_str(), "help" | "--help" | "-h") => {
            eprintln!("usage: nagi doctor [--json]");
            return Ok(0);
        }
        _ => {
            eprintln!("usage: nagi doctor [--json]");
            return Ok(2);
        }
    };
    let cwd = std::env::current_dir()?;
    let report = crate::doctor::inspect(&cwd);
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", format_report(&report));
    }
    Ok(if report.ready { 0 } else { 1 })
}

fn format_report(report: &DoctorReport) -> String {
    let mut output = format!(
        "Nagi doctor {}\n{}\n\n",
        if report.ready {
            "ready"
        } else {
            "needs attention"
        },
        report.cwd
    );
    for check in &report.checks {
        let marker = match check.status {
            CheckStatus::Pass => "[ok]",
            CheckStatus::Warning => "[!!]",
            CheckStatus::Fail => "[xx]",
        };
        output.push_str(&format!("{marker} {:<18} {}\n", check.label, check.detail));
        if let Some(remediation) = &check.remediation {
            output.push_str(&format!("     fix: {remediation}\n"));
        }
    }
    output.push_str(&format!(
        "\n{} managed provider(s) ready\n",
        report.provider_count
    ));
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_report_keeps_remediation_next_to_the_failure() {
        let report = DoctorReport {
            version: "1".into(),
            cwd: "/repo".into(),
            ready: false,
            provider_count: 0,
            checks: vec![crate::doctor::DoctorCheck {
                id: "provider".into(),
                label: "Managed runtime".into(),
                status: CheckStatus::Fail,
                detail: "none ready".into(),
                remediation: Some("Install one provider".into()),
            }],
        };
        let rendered = format_report(&report);
        assert!(rendered.contains("[xx] Managed runtime"));
        assert!(rendered.contains("fix: Install one provider"));
    }
}
