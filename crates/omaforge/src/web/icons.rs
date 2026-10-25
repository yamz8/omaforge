//! Inline SVG icons (16×16, stroked with `currentColor`; styled by `svg.icon` in style.css).

macro_rules! icon {
    ($name:ident, $body:literal) => {
        pub const $name: &str = concat!(
            r#"<svg class="icon" viewBox="0 0 16 16" aria-hidden="true">"#,
            $body,
            "</svg>"
        );
    };
}

icon!(
    LOGO,
    r#"<path d="M1.75 4.25h9.5c0 2.25-1.75 3.5-3.75 3.75v2.25h2.25v2h-7v-2h2.25V8C2.75 7.75 1.75 6.25 1.75 4.25Z"/><path d="M11.25 5.25 14.25 4"/>"#
);
icon!(
    FOLDER,
    r#"<path class="dir" d="M1.75 3.25h4l1.5 1.5h7v8.5H1.75Z"/>"#
);
icon!(
    FILE,
    r#"<path d="M3.75 1.75h5.5l3 3v9.5h-8.5Z"/><path d="M9.25 1.75v3h3"/>"#
);
icon!(
    SUBMODULE,
    r#"<path class="dir" d="M1.75 3.25h4l1.5 1.5h7v8.5H1.75Z"/><path d="M6 9.25h4M8.5 7.75 10 9.25l-1.5 1.5"/>"#
);
icon!(
    SYMLINK,
    r#"<path d="M3.75 1.75h5.5l3 3v9.5h-8.5Z"/><path d="M6 10.5c0-2 1-2.75 3-2.75M7.75 6.25 9.25 7.75 7.75 9.25"/>"#
);
icon!(
    CODE,
    r#"<path d="M5.5 4 1.75 8l3.75 4M10.5 4l3.75 4-3.75 4"/>"#
);
icon!(
    HISTORY,
    r#"<circle cx="8" cy="8" r="2.5"/><path d="M1.25 8H5.5M10.5 8h4.25"/>"#
);
icon!(
    BRANCH,
    r#"<circle cx="4.5" cy="3.5" r="1.75"/><circle cx="4.5" cy="12.5" r="1.75"/><circle cx="11.5" cy="4.5" r="1.75"/><path d="M4.5 5.25v5.5M11.5 6.25c0 3.25-7 2.25-7 4.5"/>"#
);
icon!(
    TAG,
    r#"<path d="M1.75 2.75v4.5l7 7 5.5-5.5-7-7h-4.5Z"/><circle cx="5" cy="6" r="1"/>"#
);
icon!(
    LOCK,
    r#"<rect x="3" y="7" width="10" height="7" rx="1.5"/><path d="M5.5 7V5a2.5 2.5 0 0 1 5 0v2"/>"#
);
icon!(
    BOOK,
    r#"<path d="M2 2.75h4.5A1.5 1.5 0 0 1 8 4.25v9.5A1.5 1.5 0 0 0 6.5 12.25H2ZM14 2.75H9.5A1.5 1.5 0 0 0 8 4.25v9.5a1.5 1.5 0 0 1 1.5-1.5H14Z"/>"#
);
icon!(PLUS, r#"<path d="M8 3v10M3 8h10"/>"#);
icon!(
    PULL,
    r#"<circle cx="4" cy="3.5" r="1.75"/><circle cx="4" cy="12.5" r="1.75"/><circle cx="12" cy="12.5" r="1.75"/><path d="M4 5.25v5.5M12 10.75V6.5a2 2 0 0 0-2-2H7.5M9 3 7.5 4.5 9 6"/>"#
);
icon!(
    MERGED,
    r#"<circle cx="4" cy="3.5" r="1.75"/><circle cx="4" cy="12.5" r="1.75"/><circle cx="12" cy="8" r="1.75"/><path d="M4 5.25v5.5M4 5.25c0 2 2 2.75 6.25 2.75"/>"#
);
icon!(
    CLOSED_PULL,
    r#"<circle cx="4" cy="3.5" r="1.75"/><circle cx="4" cy="12.5" r="1.75"/><circle cx="12" cy="12.5" r="1.75"/><path d="M4 5.25v5.5M12 10.75V8M10.5 2.5l3 3M13.5 2.5l-3 3"/>"#
);
icon!(CHECK, r#"<path d="M3 8.5 6.5 12 13 4.5"/>"#);
icon!(
    ALERT,
    r#"<path d="M8 1.75 14.75 13.5H1.25Z"/><path d="M8 6v3.5M8 11.5v.25"/>"#
);
icon!(COMMENT, r#"<path d="M2 3.25h12v8H8.5L5 14v-2.75H2Z"/>"#);
icon!(
    PERSON,
    r#"<circle cx="8" cy="5" r="2.75"/><path d="M2.75 14c.5-3 2.6-4.5 5.25-4.5s4.75 1.5 5.25 4.5"/>"#
);
icon!(
    ISSUE,
    r#"<circle cx="8" cy="8" r="6.25"/><circle cx="8" cy="8" r="1"/>"#
);
icon!(
    ISSUE_CLOSED,
    r#"<circle cx="8" cy="8" r="6.25"/><path d="M5.25 8.25 7.25 10.25 10.75 6"/>"#
);
icon!(
    GEAR,
    r#"<circle cx="8" cy="8" r="2.25"/><path d="M8 1.5v1.75M8 12.75v1.75M14.5 8h-1.75M3.25 8H1.5M12.6 3.4l-1.25 1.25M4.65 11.35 3.4 12.6M12.6 12.6l-1.25-1.25M4.65 4.65 3.4 3.4"/>"#
);
