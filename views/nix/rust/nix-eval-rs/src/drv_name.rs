//! Split package names at the first dash followed by a non-letter.

pub(crate) struct DrvName<'a> {
    pub(crate) name: &'a [u8],
    pub(crate) version: &'a [u8],
}

pub(crate) fn split(text: &[u8]) -> DrvName<'_> {
    for (offset, pair) in text.windows(2).enumerate() {
        if let [b'-', next] = pair
            && !next.is_ascii_alphabetic()
        {
            let (name, _) = text.split_at(offset);
            return DrvName {
                name,
                version: text.split_at(offset + 1).1,
            };
        }
    }
    DrvName {
        name: text,
        version: b"",
    }
}

#[cfg(test)]
mod tests {
    use super::split;

    #[test]
    fn trailing_dash_is_not_a_separator() {
        let parsed = split(b"hello-");
        assert_eq!(parsed.name, b"hello-");
        assert_eq!(parsed.version, b"");
    }

    #[test]
    fn first_nonletter_after_dash_starts_the_version() {
        let parsed = split(b"apache-httpd--1");
        assert_eq!(parsed.name, b"apache-httpd");
        assert_eq!(parsed.version, b"-1");
        let parsed = split(b"pkg-\xff");
        assert_eq!(parsed.name, b"pkg");
        assert_eq!(parsed.version, b"\xff");
    }
}
