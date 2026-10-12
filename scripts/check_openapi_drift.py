#!/usr/bin/env python3
"""Check that the axum router and docs/openapi.yaml list the same endpoints.

Every (method, path) the router serves must be documented under `paths:` in
docs/openapi.yaml, and every documented (method, path) must be served, unless
scripts/openapi-drift-allowlist.json says otherwise:

* "undocumented": routes the router serves on purpose without documenting
  them (each with a reason). An entry that is no longer routed, or that is
  documented after all, fails the check so the list cannot go stale.
* "pending": documented operations whose route lands in another branch of the
  same release. Once the route is in the router the entry only warns; delete
  it then.

The router is read from src/api/mod.rs without compiling it: every
`.route("path", get(..).post(..))` call is collected, with the prefix of the
`.nest("/prefix", builder_fn(..))` that mounts the function it sits in. A
construct the parser does not understand (`any(..)`, `on(..)`, `*_service`,
a nest of something other than a local builder function, a route outside a
mounted function) is an error, never a silent skip. Path parameter names are
compared too: `{id}` in the router must be `{id}` in the document.

Python 3.8+, standard library only. Exit status 0 when in sync, 1 on drift,
2 when the router or the document cannot be parsed.

    python3 scripts/check_openapi_drift.py            # check the repository
    python3 scripts/check_openapi_drift.py --self-test
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
import unittest
from typing import Dict, Iterable, List, Optional, Set, Tuple

HTTP_METHODS = ("get", "post", "put", "delete", "patch", "head", "options", "trace")
ROOT_BUILDER = "router"

Endpoint = Tuple[str, str]  # (METHOD, /path)


class ParseError(Exception):
    """The input has a shape this script does not understand."""


# ---------------------------------------------------------------------------
# Rust source
# ---------------------------------------------------------------------------

RAW_STRING = re.compile(r'b?r(#*)"')
CHAR_LITERAL = re.compile(r"'(\\.[^']*|[^'\\])'")


def _filler(literal_body: str) -> str:
    return re.sub(r"[^\n]", "_", literal_body)


def scan_rust(src: str) -> Tuple[str, str]:
    """Two views of `src`, offset for offset:

    * code: comments blanked and the inside of every string/char literal
      replaced by `_`, so brackets, quotes and `.route(` in literals or
      comments cannot confuse the structural scan;
    * text: comments blanked, literals kept, to read a literal's value.
    """
    code: List[str] = []
    text: List[str] = []
    i, n = 0, len(src)
    while i < n:
        c = src[i]
        nxt = src[i + 1] if i + 1 < n else ""
        ident_before = i > 0 and (src[i - 1].isalnum() or src[i - 1] == "_")
        raw = None if ident_before else RAW_STRING.match(src, i)
        if c == "/" and nxt == "/":
            j = src.find("\n", i)
            j = n if j == -1 else j
            blank = " " * (j - i)
            code.append(blank)
            text.append(blank)
            i = j
        elif c == "/" and nxt == "*":
            depth, j = 1, i + 2
            while j < n and depth:
                if src.startswith("/*", j):
                    depth, j = depth + 1, j + 2
                elif src.startswith("*/", j):
                    depth, j = depth - 1, j + 2
                else:
                    j += 1
            blank = re.sub(r"[^\n]", " ", src[i:j])
            code.append(blank)
            text.append(blank)
            i = j
        elif raw:
            hashes = raw.group(1)
            end = src.find('"' + hashes, raw.end())
            if end == -1:
                raise ParseError("unterminated raw string")
            j = end + 1 + len(hashes)
            code.append(src[i : raw.end()] + _filler(src[raw.end() : end]) + src[end:j])
            text.append(src[i:j])
            i = j
        elif c == '"':
            j = i + 1
            while j < n and src[j] != '"':
                j += 2 if src[j] == "\\" else 1
            if j >= n:
                raise ParseError("unterminated string literal")
            code.append('"' + _filler(src[i + 1 : j]) + '"')
            text.append(src[i : j + 1])
            i = j + 1
        elif c == "'" and CHAR_LITERAL.match(src, i):
            literal = CHAR_LITERAL.match(src, i).group(0)
            code.append("'" + _filler(literal[1:-1]) + "'")
            text.append(literal)
            i += len(literal)
        else:  # code, or the quote of a lifetime ('a)
            code.append(c)
            text.append(c)
            i += 1
    code_s, text_s = "".join(code), "".join(text)
    assert len(code_s) == len(text_s) == len(src)
    return code_s, text_s


def matching_close(code: str, open_at: int) -> int:
    """Index of the bracket closing the one at `open_at` in a scanned `code`."""
    pairs = {"(": ")", "{": "}", "[": "]"}
    stack = [pairs[code[open_at]]]
    for i in range(open_at + 1, len(code)):
        c = code[i]
        if c in pairs:
            stack.append(pairs[c])
        elif c in ")}]":
            if c != stack[-1]:
                raise ParseError(f"unbalanced {c!r} at offset {i}")
            stack.pop()
            if not stack:
                return i
    raise ParseError(f"unclosed {code[open_at]!r} at offset {open_at}")


Span = Tuple[int, int]


def split_args(code: str, span: Span) -> List[Span]:
    """Spans of a call's top-level, comma-separated arguments."""
    parts: List[Span] = []
    depth, start = 0, span[0]
    for i in range(span[0], span[1]):
        c = code[i]
        if c in "([{":
            depth += 1
        elif c in ")]}":
            depth -= 1
        elif c == "," and depth == 0:
            parts.append((start, i))
            start = i + 1
    if code[start : span[1]].strip():
        parts.append((start, span[1]))
    return parts


def rust_functions(code: str) -> Tuple[Dict[str, Span], Set[str]]:
    """Body span of each `fn name`, and the names defined more than once
    (test helpers may repeat a name)."""
    funcs: Dict[str, Span] = {}
    repeated: Set[str] = set()
    for m in re.finditer(r"\bfn\s+([A-Za-z_]\w*)\s*(<[^>{]*>)?\s*\(", code):
        params_close = matching_close(code, m.end() - 1)
        body_open = code.find("{", params_close)
        semi = code.find(";", params_close)
        if body_open == -1 or (semi != -1 and semi < body_open):
            continue  # a declaration without a body
        if m.group(1) in funcs:
            repeated.add(m.group(1))
        funcs[m.group(1)] = (body_open + 1, matching_close(code, body_open))
    return funcs, repeated


def calls(code: str, span: Span, name: str) -> List[Span]:
    """Argument spans of every `.name(...)` call inside `span`."""
    found = []
    pattern = re.compile(r"\.\s*" + re.escape(name) + r"\s*\(")
    for m in pattern.finditer(code, span[0], span[1]):
        open_at = m.end() - 1
        found.append((open_at + 1, matching_close(code, open_at)))
    return found


def string_literal(text: str, span: Span, what: str) -> str:
    arg = text[span[0] : span[1]].strip()
    m = re.fullmatch(r'"((?:[^"\\]|\\.)*)"', arg)
    if not m:
        raise ParseError(f"{what}: expected a string literal, got {arg[:60]!r}")
    return m.group(1)


def called_fn(code: str, span: Span) -> Optional[str]:
    """`name` when the expression in `span` starts with a call `name(..)`."""
    m = re.match(r"\s*([A-Za-z_]\w*)\s*\(", code[span[0] : span[1]])
    return m.group(1) if m else None


UNSUPPORTED_METHOD_ROUTERS = re.compile(
    r"(?<![\w])(any|on|any_service|get_service|post_service|put_service|delete_service|"
    r"patch_service|on_service|route_service|nest_service|fallback_service)\s*\("
)
METHOD_CALL = re.compile(r"(?<![\w])(" + "|".join(HTTP_METHODS) + r")\s*\(")


def methods_of(method_router: str, where: str) -> List[str]:
    bad = UNSUPPORTED_METHOD_ROUTERS.search(method_router)
    if bad:
        raise ParseError(f"{where}: unsupported method router {bad.group(1)}(..); teach this script")
    found = [m.group(1).upper() for m in METHOD_CALL.finditer(method_router)]
    if not found:
        raise ParseError(f"{where}: no HTTP method in {method_router.strip()[:80]!r}")
    return found


def router_endpoints(rust_src: str) -> Set[Endpoint]:
    """Every (METHOD, path) served by the router built in `rust_src`."""
    code, text = scan_rust(rust_src)
    funcs, repeated = rust_functions(code)
    if ROOT_BUILDER not in funcs:
        raise ParseError(f"no `fn {ROOT_BUILDER}` in the router source")

    for name, body in funcs.items():
        for unsupported in ("route_service", "nest_service"):
            if calls(code, body, unsupported):
                raise ParseError(f"fn {name}: .{unsupported}(..) is not supported; teach this script")

    # Prefix of every builder function, walking .nest and .merge from the root.
    prefixes: Dict[str, str] = {ROOT_BUILDER: ""}
    queue = [ROOT_BUILDER]
    while queue:
        name = queue.pop()
        body = funcs[name]
        mounts: List[Tuple[str, str]] = []
        for args in calls(code, body, "nest"):
            parts = split_args(code, args)
            if len(parts) != 2:
                raise ParseError(f"fn {name}: .nest(..) with {len(parts)} arguments")
            prefix = string_literal(text, parts[0], f"fn {name}: .nest prefix")
            target = called_fn(code, parts[1])
            if target not in funcs:
                raise ParseError(
                    f"fn {name}: .nest({prefix!r}, ..) does not call a builder fn in this file"
                )
            mounts.append((prefixes[name] + prefix, target))
        for args in calls(code, body, "merge"):
            target = called_fn(code, args)
            if target in funcs:
                mounts.append((prefixes[name], target))
        for prefix, target in mounts:
            if target in prefixes and prefixes[target] != prefix:
                raise ParseError(f"fn {target} is mounted at two prefixes")
            if target not in prefixes:
                prefixes[target] = prefix
                queue.append(target)

    routes_by_fn = {name: calls(code, body, "route") for name, body in funcs.items()}
    builders = set(prefixes) | {name for name, routes in routes_by_fn.items() if routes}
    if repeated & builders:
        raise ParseError(f"router builder fn defined more than once: {sorted(repeated & builders)}")

    endpoints: Set[Endpoint] = set()
    for name, routes in routes_by_fn.items():
        if routes and name not in prefixes:
            raise ParseError(f"fn {name} adds routes but is not mounted from fn {ROOT_BUILDER}")
        for args in routes:
            parts = split_args(code, args)
            if len(parts) != 2:
                raise ParseError(f"fn {name}: .route(..) with {len(parts)} arguments")
            path = string_literal(text, parts[0], f"fn {name}: .route path")
            if not path.startswith("/"):
                raise ParseError(f"fn {name}: route path {path!r} does not start with /")
            method_router = code[parts[1][0] : parts[1][1]]
            for method in methods_of(method_router, f"fn {name}: route {path}"):
                endpoints.add((method, join_path(prefixes[name], path)))
    return endpoints


def join_path(prefix: str, path: str) -> str:
    if prefix and path == "/":
        return prefix
    return prefix.rstrip("/") + path


# ---------------------------------------------------------------------------
# OpenAPI document
# ---------------------------------------------------------------------------

PATH_KEY = re.compile(r"""^  (?P<q>['"]?)(?P<path>/[^'"]*?)(?P=q):(\s.*)?$""")
OPERATION_KEY = re.compile(r"^    (" + "|".join(HTTP_METHODS) + r"):(\s.*)?$")


def openapi_endpoints(yaml_text: str) -> Set[Endpoint]:
    """Every (METHOD, path) under the top-level `paths:` of an OpenAPI YAML
    document written in block style with two-space indentation."""
    lines = yaml_text.splitlines()
    try:
        start = next(i for i, line in enumerate(lines) if re.match(r"^paths:\s*(#.*)?$", line))
    except StopIteration:
        raise ParseError("no top-level `paths:` key in the OpenAPI document")

    endpoints: Set[Endpoint] = set()
    current: Optional[str] = None
    for number, line in enumerate(lines[start + 1 :], start + 2):
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        if not line.startswith(" "):
            break  # the next top-level key
        if line.startswith("  ") and not line.startswith("   "):
            m = PATH_KEY.match(line)
            if not m:
                raise ParseError(f"openapi line {number}: expected a path key, got {line.strip()!r}")
            current = m.group("path")
            continue
        if line.startswith("    ") and not line.startswith("     "):
            if re.match(r"^    \$ref:", line):
                raise ParseError(f"openapi line {number}: $ref path items are not supported")
            m = OPERATION_KEY.match(line)
            if m:
                if current is None:
                    raise ParseError(f"openapi line {number}: operation outside a path")
                endpoints.add((m.group(1).upper(), current))
    return endpoints


# ---------------------------------------------------------------------------
# Comparison
# ---------------------------------------------------------------------------


def shape(path: str) -> str:
    """The path with parameter names erased: /jobs/{id} -> /jobs/{}."""
    return re.sub(r"\{\*?[^}]*\}", "{}", path)


def load_allowlist(path: Optional[str]) -> Tuple[Dict[Endpoint, str], Dict[Endpoint, str]]:
    if not path or not os.path.exists(path):
        return {}, {}
    with open(path, encoding="utf-8") as fh:
        data = json.load(fh)
    lists = []
    for section in ("undocumented", "pending"):
        entries: Dict[Endpoint, str] = {}
        for entry in data.get(section, []):
            method, route, reason = entry.get("method"), entry.get("path"), entry.get("reason")
            if not (method and route and reason):
                raise ParseError(f"allowlist {section}: every entry needs method, path and reason")
            entries[(method.upper(), route)] = reason
        lists.append(entries)
    unknown = set(data) - {"undocumented", "pending", "_comment"}
    if unknown:
        raise ParseError(f"allowlist: unknown keys {sorted(unknown)}")
    return lists[0], lists[1]


def compare(
    routed: Set[Endpoint],
    documented: Set[Endpoint],
    undocumented_ok: Dict[Endpoint, str],
    pending: Dict[Endpoint, str],
) -> Tuple[List[str], List[str]]:
    """(errors, warnings) for the two endpoint sets."""
    errors: List[str] = []
    warnings: List[str] = []

    doc_by_shape: Dict[Tuple[str, str], str] = {(m, shape(p)): p for m, p in documented}
    route_by_shape: Dict[Tuple[str, str], str] = {(m, shape(p)): p for m, p in routed}

    for method, path in sorted(routed, key=lambda e: (e[1], e[0])):
        doc_path = doc_by_shape.get((method, shape(path)))
        if (method, path) in undocumented_ok:
            if doc_path is not None:
                errors.append(
                    f"{method} {path} is documented now; drop it from the allowlist's undocumented list"
                )
            continue
        if doc_path is None:
            errors.append(f"{method} {path} is routed but not documented in openapi.yaml")
        elif doc_path != path:
            errors.append(
                f"{method} {path} is documented as {doc_path}: use the router's parameter names"
            )

    for method, path in sorted(documented, key=lambda e: (e[1], e[0])):
        if (method, shape(path)) in route_by_shape:
            if (method, path) in pending:
                warnings.append(
                    f"{method} {path} is routed now; delete it from the allowlist's pending list"
                )
            continue
        if (method, path) in pending:
            continue
        errors.append(f"{method} {path} is documented in openapi.yaml but not routed")

    for method, path in sorted(undocumented_ok):
        if (method, shape(path)) not in route_by_shape:
            errors.append(
                f"allowlist entry {method} {path} (undocumented) is not routed; delete the entry"
            )
    for method, path in sorted(pending):
        if (method, shape(path)) not in doc_by_shape:
            errors.append(
                f"allowlist entry {method} {path} (pending) is not documented; document it or delete the entry"
            )
    return errors, warnings


def run(router_file: str, openapi_file: str, allowlist_file: Optional[str]) -> int:
    try:
        with open(router_file, encoding="utf-8") as fh:
            routed = router_endpoints(fh.read())
        with open(openapi_file, encoding="utf-8") as fh:
            documented = openapi_endpoints(fh.read())
        undocumented_ok, pending = load_allowlist(allowlist_file)
    except (OSError, ParseError, ValueError) as e:
        print(f"openapi drift check could not run: {e}", file=sys.stderr)
        return 2

    errors, warnings = compare(routed, documented, undocumented_ok, pending)
    annotate = os.environ.get("GITHUB_ACTIONS") == "true"
    for w in warnings:
        print(f"::warning::{w}" if annotate else f"warning: {w}")
    for e in errors:
        print(f"::error::{e}" if annotate else f"error: {e}")
    print(
        f"{len(routed)} routed operations, {len(documented)} documented, "
        f"{len(undocumented_ok)} allowed undocumented, {len(pending)} pending: "
        + ("in sync" if not errors else f"{len(errors)} problem(s)")
    )
    return 1 if errors else 0


# ---------------------------------------------------------------------------
# Self-test
# ---------------------------------------------------------------------------

SAMPLE_ROUTER = r'''
pub fn router(state: AppState) -> Router {
    // .route("/commented-out", get(nothing))
    Router::new()
        .route("/health", get(handlers::health::health_check))
        .route("/metrics", get(handlers::metrics::prometheus_metrics)) // trailing "//"
        .nest(
            "/api/v1",
            api_v1_router(state.clone())
                .layer(axum::middleware::from_fn(api_versioning_middleware)),
        )
        .with_state(state)
}

fn api_v1_router(state: AppState) -> Router<AppState> {
    let note = "a // string with .route(\"/fake\", get(x)) inside";
    let c = '"';
    /* .route("/blocked", post(x)) */
    let public = Router::new()
        .route(
            "/jobs/bulk",
            post(handlers::jobs::bulk_enqueue).layer(DefaultBodyLimit::max(limit())),
        )
        .route("/jobs/{id}", get(handlers::jobs::get))
        .route("/jobs/{id}", delete(handlers::jobs::cancel))
        .route(
            "/admin/organizations",
            get(handlers::admin::list).post(handlers::admin::create),
        )
        .route("/admin/organizations/{id}", axum::routing::patch(handlers::admin::update));
    public.merge(more_routes(state))
}

fn more_routes(state: AppState) -> Router<AppState> {
    Router::new().route("/schedules/{id}/history", get(handlers::schedules::history))
}
'''

SAMPLE_OPENAPI = """openapi: 3.1.0
info:
  title: x
  description: |
    paths:
      /not-a-path:
        get:
paths:
  /health:
    get:
      summary: h
  /metrics:
    get:
      responses: {}
  /api/v1/jobs/bulk:
    post:
      summary: b
  "/api/v1/jobs/{id}":
    parameters:
      - name: id
    get:
      summary: g
    delete:
      summary: d
  /api/v1/admin/organizations:
    get: {}
    post: {}
  /api/v1/admin/organizations/{id}:
    patch: {}
  /api/v1/schedules/{id}/history:
    get: {}
components:
  schemas:
    get:
      type: object
"""


class SelfTest(unittest.TestCase):
    def test_router_routes_are_collected_with_their_nest_prefix(self):
        self.assertEqual(
            router_endpoints(SAMPLE_ROUTER),
            {
                ("GET", "/health"),
                ("GET", "/metrics"),
                ("POST", "/api/v1/jobs/bulk"),
                ("GET", "/api/v1/jobs/{id}"),
                ("DELETE", "/api/v1/jobs/{id}"),
                ("GET", "/api/v1/admin/organizations"),
                ("POST", "/api/v1/admin/organizations"),
                ("PATCH", "/api/v1/admin/organizations/{id}"),
                ("GET", "/api/v1/schedules/{id}/history"),
            },
        )

    def test_openapi_operations_are_collected_from_paths_only(self):
        self.assertEqual(openapi_endpoints(SAMPLE_OPENAPI), router_endpoints(SAMPLE_ROUTER))

    def test_in_sync_has_no_errors(self):
        errors, warnings = compare(
            router_endpoints(SAMPLE_ROUTER), openapi_endpoints(SAMPLE_OPENAPI), {}, {}
        )
        self.assertEqual((errors, warnings), ([], []))

    def test_an_undocumented_route_is_an_error(self):
        src = SAMPLE_ROUTER.replace(
            '.route("/jobs/{id}", get(handlers::jobs::get))',
            '.route("/jobs/{id}", get(handlers::jobs::get))\n'
            '        .route("/organizations/{id}/members", get(handlers::organizations::members))',
        )
        errors, _ = compare(router_endpoints(src), openapi_endpoints(SAMPLE_OPENAPI), {}, {})
        self.assertEqual(
            errors,
            ["GET /api/v1/organizations/{id}/members is routed but not documented in openapi.yaml"],
        )
        # ...unless the allowlist says why.
        allowed = {("GET", "/api/v1/organizations/{id}/members"): "internal"}
        errors, _ = compare(router_endpoints(src), openapi_endpoints(SAMPLE_OPENAPI), allowed, {})
        self.assertEqual(errors, [])

    def test_a_documented_but_unrouted_operation_is_an_error_unless_pending(self):
        doc = SAMPLE_OPENAPI.replace("components:", "  /api/v1/webhooks/deliveries:\n    get: {}\ncomponents:")
        errors, _ = compare(router_endpoints(SAMPLE_ROUTER), openapi_endpoints(doc), {}, {})
        self.assertEqual(
            errors, ["GET /api/v1/webhooks/deliveries is documented in openapi.yaml but not routed"]
        )
        pending = {("GET", "/api/v1/webhooks/deliveries"): "lands in another branch"}
        errors, warnings = compare(router_endpoints(SAMPLE_ROUTER), openapi_endpoints(doc), {}, pending)
        self.assertEqual((errors, warnings), ([], []))

    def test_a_pending_entry_that_is_routed_now_only_warns(self):
        pending = {("GET", "/api/v1/jobs/{id}"): "landed"}
        errors, warnings = compare(
            router_endpoints(SAMPLE_ROUTER), openapi_endpoints(SAMPLE_OPENAPI), {}, pending
        )
        self.assertEqual(errors, [])
        self.assertEqual(len(warnings), 1)

    def test_stale_allowlist_entries_are_errors(self):
        routed, documented = router_endpoints(SAMPLE_ROUTER), openapi_endpoints(SAMPLE_OPENAPI)
        errors, _ = compare(routed, documented, {("GET", "/gone"): "x"}, {})
        self.assertIn("allowlist entry GET /gone (undocumented) is not routed; delete the entry", errors)
        errors, _ = compare(routed, documented, {("GET", "/health"): "x"}, {})
        self.assertIn(
            "GET /health is documented now; drop it from the allowlist's undocumented list", errors
        )
        errors, _ = compare(routed, documented, {}, {("GET", "/nowhere"): "x"})
        self.assertIn(
            "allowlist entry GET /nowhere (pending) is not documented; document it or delete the entry",
            errors,
        )

    def test_wrong_method_and_parameter_names_are_errors(self):
        doc = SAMPLE_OPENAPI.replace("  /api/v1/admin/organizations/{id}:\n    patch:", "  /api/v1/admin/organizations/{id}:\n    put:")
        errors, _ = compare(router_endpoints(SAMPLE_ROUTER), openapi_endpoints(doc), {}, {})
        self.assertEqual(
            sorted(errors),
            sorted(
                [
                    "PATCH /api/v1/admin/organizations/{id} is routed but not documented in openapi.yaml",
                    "PUT /api/v1/admin/organizations/{id} is documented in openapi.yaml but not routed",
                ]
            ),
        )
        doc = SAMPLE_OPENAPI.replace("/api/v1/schedules/{id}/history", "/api/v1/schedules/{schedule_id}/history")
        errors, _ = compare(router_endpoints(SAMPLE_ROUTER), openapi_endpoints(doc), {}, {})
        self.assertEqual(
            errors,
            [
                "GET /api/v1/schedules/{id}/history is documented as "
                "/api/v1/schedules/{schedule_id}/history: use the router's parameter names"
            ],
        )

    def test_unsupported_constructs_fail_loudly(self):
        for snippet in (
            '.route("/x", any(handler))',
            '.route("/x", get_service(ServeDir::new(".")))',
            '.route_service("/x", svc)',
            '.nest("/x", some_router)',
            '.route(PATH, get(handler))',
        ):
            src = SAMPLE_ROUTER.replace(".with_state(state)", snippet + "\n        .with_state(state)")
            with self.assertRaises(ParseError, msg=snippet):
                router_endpoints(src)
        unmounted = SAMPLE_ROUTER + '\nfn orphan() -> Router { Router::new().route("/o", get(h)) }\n'
        with self.assertRaises(ParseError):
            router_endpoints(unmounted)
        with self.assertRaises(ParseError):
            openapi_endpoints(SAMPLE_OPENAPI.replace("  /health:\n", "  /health:\n    $ref: '#/x'\n"))
        with self.assertRaises(ParseError):
            openapi_endpoints(SAMPLE_OPENAPI.replace("  /health:\n", "  health:\n"))

    def test_crlf_documents_parse(self):
        self.assertEqual(
            openapi_endpoints(SAMPLE_OPENAPI.replace("\n", "\r\n")), openapi_endpoints(SAMPLE_OPENAPI)
        )


def main(argv: List[str]) -> int:
    repo = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--router", default=os.path.join(repo, "src", "api", "mod.rs"))
    parser.add_argument("--openapi", default=os.path.join(repo, "docs", "openapi.yaml"))
    parser.add_argument(
        "--allowlist", default=os.path.join(repo, "scripts", "openapi-drift-allowlist.json")
    )
    parser.add_argument("--self-test", action="store_true", help="run the script's own tests")
    args = parser.parse_args(argv)
    if args.self_test:
        result = unittest.TextTestRunner(verbosity=1).run(
            unittest.defaultTestLoader.loadTestsFromTestCase(SelfTest)
        )
        return 0 if result.wasSuccessful() else 1
    return run(args.router, args.openapi, args.allowlist)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
