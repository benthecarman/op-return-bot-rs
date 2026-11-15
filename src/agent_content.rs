use crate::config::PaymentConfig;

const API_DOCS_URL: &str = "https://github.com/benthecarman/OP-RETURN-Bot/blob/master/docs/API.md";
const SOURCE_URL: &str = "https://github.com/benthecarman/OP-RETURN-Bot";

/// One question and answer. The home page, its JSON-LD, the markdown home
/// page, and `/llms.txt` all render the same list, so they cannot disagree.
#[derive(Clone, Debug)]
pub struct Faq {
    pub question: &'static str,
    pub answer: String,
}

/// Builds the FAQ from the live payment settings, so the prices and limits
/// it states are the ones the service charges.
#[must_use]
pub fn faq(payments: &PaymentConfig, base: &str) -> Vec<Faq> {
    let domain = base
        .split_once("://")
        .map_or(base, |(_, rest)| rest)
        .trim_end_matches('/');
    vec![
        Faq {
            question: "What does OP_RETURN Bot do?",
            answer: "OP_RETURN Bot writes a message or a file into the Bitcoin \
                     blockchain in an OP_RETURN output. You type the message or \
                     upload the file, then pay one payment request. The bot \
                     builds, signs, and broadcasts the transaction. You do not \
                     need a wallet that can make OP_RETURN outputs."
                .to_owned(),
        },
        Faq {
            question: "How much does it cost?",
            answer: format!(
                "You pay the network fee for the transaction plus a service fee \
                 of {} sats. The network fee depends on the size of the message \
                 and on current fee rates, so the payment page shows the exact \
                 price before you pay. A private message or a file costs {} sats \
                 more. A message larger than 80 bytes costs {} sats more.",
                group_digits(payments.application_fee_sats),
                group_digits(payments.private_fee_sats),
                group_digits(payments.non_standard_fee_sats),
            ),
        },
        Faq {
            question: "How large can a message be?",
            answer: format!(
                "Up to {} bytes. Text is stored as UTF-8, and a file is written \
                 byte for byte. The bot sends each transaction to Bitcoin Core \
                 and to MARA Slipstream, so messages larger than the old 80-byte \
                 limit also reach a miner.",
                group_digits(payments.message_max_bytes as u64),
            ),
        },
        Faq {
            question: "How do I pay?",
            answer: format!(
                "With Lightning or with on-chain bitcoin. The payment page shows \
                 a unified QR code that works with either. It also shows a \
                 BOLT11 Lightning invoice, a BOLT12 offer when one is available, \
                 and an on-chain address. The Lightning invoice expires after \
                 {}. The on-chain address accepts payment for {}.",
                duration(u64::from(payments.invoice_expiry_seconds)),
                duration(payments.on_chain_expiry_seconds),
            ),
        },
        Faq {
            question: "Does OP_RETURN Bot hold my bitcoin?",
            answer: "No. There is no account, sign-up, or balance. Each payment \
                     pays for one transaction."
                .to_owned(),
        },
        Faq {
            question: "How long until my message is in a block?",
            answer: "The bot broadcasts the transaction as soon as your payment \
                     arrives. It pays a fee rate above the current fastest \
                     estimate, so the transaction usually confirms within a few \
                     blocks. The success page links to the transaction on \
                     mempool.space."
                .to_owned(),
        },
        Faq {
            question: "Who can see my message?",
            answer: "Everyone. Data in the Bitcoin blockchain is public and \
                     permanent. By default the bot also posts text messages to \
                     its Twitter and Nostr accounts. Select Private to stop this. \
                     The bot never posts files."
                .to_owned(),
        },
        Faq {
            question: "Can a program or an AI agent use OP_RETURN Bot?",
            answer: format!(
                "Yes. Use the REST API or the MCP server at {base}/mcp. Neither \
                 needs an API key or an account. The API documentation is at \
                 {API_DOCS_URL}."
            ),
        },
        Faq {
            question: "What is the NIP-05 service?",
            answer: format!(
                "The bot can give your Nostr public key a NIP-05 identifier such \
                 as name@{domain}. It writes the name and the key into the \
                 Bitcoin blockchain in an OP_RETURN output. Set it up at \
                 {base}/nip5."
            ),
        },
    ]
}

/// JSON-LD for the home page: the service itself.
#[must_use]
pub fn app_json_ld(base: &str) -> String {
    script_json(&serde_json::json!({
        "@context": "https://schema.org",
        "@type": "WebApplication",
        "@id": format!("{base}/#app"),
        "name": "OP_RETURN Bot",
        "url": format!("{base}/"),
        "description": DESCRIPTION,
        "applicationCategory": "FinanceApplication",
        "operatingSystem": "Any",
        "author": {
            "@type": "Person",
            "name": "benthecarman",
            "url": "https://github.com/benthecarman"
        },
        "sameAs": [
            SOURCE_URL,
            "https://twitter.com/OP_RETURN_BOT",
            "https://t.me/OP_RETURN_BOT"
        ]
    }))
}

/// JSON-LD for the FAQ page.
#[must_use]
pub fn faq_json_ld(faq: &[Faq], base: &str) -> String {
    script_json(&serde_json::json!({
        "@context": "https://schema.org",
        "@type": "FAQPage",
        "@id": format!("{base}/faq"),
        "about": {"@id": format!("{base}/#app")},
        "mainEntity": faq.iter().map(|entry| serde_json::json!({
            "@type": "Question",
            "name": entry.question,
            "acceptedAnswer": {"@type": "Answer", "text": entry.answer}
        })).collect::<Vec<_>>()
    }))
}

/// Serializes JSON for a `<script>` element. `<` is escaped so that text in
/// the JSON cannot close the element.
fn script_json(value: &serde_json::Value) -> String {
    value.to_string().replace('<', "\\u003c")
}

pub const DESCRIPTION: &str = "Write a message or a file into the Bitcoin \
    blockchain in an OP_RETURN output. Pay with Lightning or on-chain bitcoin. \
    No account needed.";

/// The markdown home page, also served as `/llms.txt`.
#[must_use]
pub fn index_markdown(faq: &[Faq], base: &str) -> String {
    let mut out = format!(
        "# OP_RETURN Bot\n\n\
         > OP_RETURN Bot writes messages and files into the Bitcoin blockchain \
         in OP_RETURN outputs. You pay with Lightning or on-chain bitcoin. There \
         is no account and no API key.\n\n\
         ## Use it\n\n\
         - [Website]({base}/): type a message or upload a file, then pay\n\
         - [NIP-05]({base}/nip5): get a Nostr identifier written into the blockchain\n\
         - [FAQ]({base}/faq): prices, limits, payment, and privacy\n\
         - [MCP server]({base}/mcp): streamable HTTP; discovery at \
         [/.well-known/mcp.json]({base}/.well-known/mcp.json)\n\
         - [REST API docs]({API_DOCS_URL}): `POST /api/create`, `POST /api/unified`, \
         `GET /api/status/{{rHash}}`, and `GET /api/view/{{txId}}`\n\
         - [Agent skill]({base}/.well-known/agent-skills/op-return-bot/SKILL.md)\n\
         - [API catalog]({base}/.well-known/api-catalog)\n\
         - [Authentication]({base}/auth.md): none\n\
         - [Source code]({SOURCE_URL})\n\n\
         ## FAQ\n"
    );
    for entry in faq {
        out.push_str("\n### ");
        out.push_str(entry.question);
        out.push_str("\n\n");
        out.push_str(&entry.answer);
        out.push('\n');
    }
    out
}

fn group_digits(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

fn duration(seconds: u64) -> String {
    let (count, unit) = if seconds >= 86_400 && seconds.is_multiple_of(86_400) {
        (seconds / 86_400, "day")
    } else if seconds >= 3_600 && seconds.is_multiple_of(3_600) {
        (seconds / 3_600, "hour")
    } else if seconds >= 60 && seconds.is_multiple_of(60) {
        (seconds / 60, "minute")
    } else {
        (seconds, "second")
    };
    if count == 1 {
        format!("1 {unit}")
    } else {
        format!("{count} {unit}s")
    }
}

pub const AUTH_MD: &str = r"# OP_RETURN Bot auth.md

OP_RETURN Bot has no authentication. It has no API keys, OAuth flow, bearer
tokens, user registration, or scopes. All REST and MCP endpoints are public.
Each write request returns a Lightning or unified Bitcoin payment request.
Payment authorizes that one write.

REST API documentation:
https://github.com/benthecarman/OP-RETURN-Bot/blob/master/docs/API.md
";

pub const SKILL_MD: &str = r"---
name: op-return-bot
description: Write messages to Bitcoin OP_RETURN outputs with a paid REST API or MCP server.
license: MIT
---

# OP_RETURN Bot

Use the streamable HTTP MCP server at `https://opreturnbot.com/mcp`, or use the
REST API. No authentication is required.

1. Call `POST /api/create` or `POST /api/unified` with `message` and optional
   `noTwitter` fields.
2. Pay the returned request.
3. Poll `GET /api/status/{rHash}` until it returns a Bitcoin transaction ID.
4. Read the message with `GET /api/view/{txId}`.

The maximum message size is 99,000 bytes.
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_numbers_and_durations() {
        assert_eq!(group_digits(0), "0");
        assert_eq!(group_digits(999), "999");
        assert_eq!(group_digits(1_337), "1,337");
        assert_eq!(group_digits(99_000), "99,000");
        assert_eq!(group_digits(1_000_000), "1,000,000");
        assert_eq!(duration(300), "5 minutes");
        assert_eq!(duration(604_800), "7 days");
        assert_eq!(duration(3_600), "1 hour");
        assert_eq!(duration(90), "90 seconds");
    }

    #[test]
    fn faq_uses_the_configured_prices() {
        let entries = faq(&PaymentConfig::default(), "https://example.com");
        let all = entries
            .iter()
            .map(|entry| entry.answer.as_str())
            .collect::<String>();
        assert!(all.contains("1,337 sats"));
        assert!(all.contains("99,000 bytes"));
        assert!(all.contains("5 minutes"));
        assert!(all.contains("name@example.com"));
    }

    #[test]
    fn json_ld_cannot_close_its_script_element() {
        let entries = vec![Faq {
            question: "q",
            answer: "</script><script>alert(1)</script>".to_owned(),
        }];
        let json = faq_json_ld(&entries, "https://example.com");
        assert!(!json.contains('<'));
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            parsed["mainEntity"][0]["acceptedAnswer"]["text"],
            "</script><script>alert(1)</script>"
        );
    }
}
