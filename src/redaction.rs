const MAX_OPERATIONAL_MESSAGE_CHARS: usize = 1_024;

/// Reduce un mensaje no confiable antes de enviarlo a tracing, TUI o stderr.
/// Los artefactos privados tipados conservan sus identificadores para poder
/// reconciliar; esta función es exclusivamente para texto humano operativo.
pub fn sanitize_operational_message(message: &str) -> String {
    let normalized = message
        .chars()
        .map(|character| {
            if character.is_control() && !matches!(character, '\t') {
                ' '
            } else {
                character
            }
        })
        .take(MAX_OPERATIONAL_MESSAGE_CHARS)
        .collect::<String>();
    let lowercase = normalized.to_ascii_lowercase();
    const SECRET_MARKERS: [&str; 12] = [
        "bearer ",
        "authorization:",
        "access_token=",
        "access_token ",
        "\"access_token\":",
        "refresh_token=",
        "refresh_token ",
        "\"refresh_token\":",
        "password=",
        "\"password\":",
        "iol_password=",
        "client_secret=",
    ];
    if SECRET_MARKERS
        .iter()
        .any(|marker| lowercase.contains(marker))
    {
        return "Detalle sensible ocultado".into();
    }

    normalized
        .split_whitespace()
        .map(sanitize_word)
        .collect::<Vec<_>>()
        .join(" ")
}

fn sanitize_word(word: &str) -> String {
    if word.contains('@') {
        return "••••@••••".into();
    }

    let mut output = String::with_capacity(word.len());
    let mut characters = word.chars().peekable();
    while let Some(character) = characters.next() {
        if character.is_ascii_digit() {
            let mut digits = String::from(character);
            while characters.peek().is_some_and(|next| next.is_ascii_digit()) {
                digits.push(characters.next().expect("peek confirmó un dígito"));
            }
            if digits.len() >= 6 {
                output.push_str("••••");
                output.push_str(&digits[digits.len() - 4..]);
            } else {
                output.push_str(&digits);
            }
        } else {
            output.push(character);
        }
    }
    output
}

pub fn masked_identifier(value: &str) -> String {
    let suffix = value.chars().rev().take(4).collect::<Vec<_>>();
    format!("••••{}", suffix.into_iter().rev().collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_secrets_accounts_emails_controls_and_unbounded_text() {
        assert_eq!(
            sanitize_operational_message("Authorization: Bearer top-secret"),
            "Detalle sensible ocultado"
        );
        assert_eq!(
            sanitize_operational_message("cuenta 2033590 usuario persona@example.com"),
            "cuenta ••••3590 usuario ••••@••••"
        );
        assert_eq!(
            sanitize_operational_message("estado 401\nreintentando"),
            "estado 401 reintentando"
        );
        assert_eq!(
            sanitize_operational_message("control abc\0def"),
            "control abc def"
        );
        assert_eq!(
            sanitize_operational_message("refs abc123 12345 123456"),
            "refs abc123 12345 ••••3456"
        );
        assert!(
            sanitize_operational_message(&"x".repeat(2_000))
                .chars()
                .count()
                <= 1_024
        );
    }

    #[test]
    fn short_external_identifiers_are_explicitly_masked() {
        assert_eq!(masked_identifier("42"), "••••42");
        assert_eq!(masked_identifier("123456789"), "••••6789");
    }
}
