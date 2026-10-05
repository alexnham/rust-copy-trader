#!/usr/bin/env python3
"""Local, read-only SQLite workbench for the copy-trader journal."""
import argparse
from contextlib import contextmanager
import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import sqlite3
import time
from urllib.parse import parse_qs, urlparse

ROOT = Path(__file__).resolve().parent
MAX_ROWS = 500


def identifier(value):
    return '"' + value.replace('"', '""') + '"'


def cell(value):
    if isinstance(value, bytes):
        return f"[binary: {len(value):,} bytes]"
    if isinstance(value, str) and len(value) > 32768:
        return value[:32768] + "\n[display truncated at 32,768 characters]"
    # Keep SQLite 64-bit integers exact in JavaScript.
    if isinstance(value, int) and abs(value) > 9007199254740991:
        return str(value)
    return value


class Database:
    def __init__(self, path):
        self.path = Path(path).resolve(strict=True)
        if not self.path.is_file():
            raise ValueError("Database path must be a file")

    @contextmanager
    def connect(self):
        connection = sqlite3.connect(self.path.as_uri() + "?mode=ro", uri=True, timeout=0.25)
        try:
            connection.execute("PRAGMA query_only = ON")
            connection.setlimit(sqlite3.SQLITE_LIMIT_LENGTH, 1_000_000)
            connection.setlimit(sqlite3.SQLITE_LIMIT_SQL_LENGTH, 50_000)
            deadline = time.monotonic() + 2
            connection.set_progress_handler(lambda: int(time.monotonic() > deadline), 1000)
            yield connection
        finally:
            connection.close()

    @staticmethod
    def tables(connection):
        return dict(connection.execute("SELECT name, sql FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name"))

    @staticmethod
    def columns(connection, table):
        return [dict(zip(("cid", "name", "type", "not_null", "default", "primary_key"), row))
                for row in connection.execute(f"PRAGMA table_info({identifier(table)})")]

    def catalog(self):
        with self.connect() as connection:
            tables = []
            for name, sql in self.tables(connection).items():
                count = connection.execute(f"SELECT count(*) FROM {identifier(name)}").fetchone()[0]
                tables.append({"name": name, "count": count, "sql": sql, "columns": self.columns(connection, name)})
            attempts = []
            if "copy_attempts" in self.tables(connection):
                attempts = [{"target": target, "status": status, "count": count} for target, status, count in
                            connection.execute("SELECT execution_target,status,count(*) FROM copy_attempts GROUP BY 1,2")]
            return {"filename": self.path.name, "size": self.path.stat().st_size, "tables": tables,
                    "attempts": attempts, "read_only": True}

    @staticmethod
    def result(cursor, limit=MAX_ROWS):
        columns = [column[0] for column in cursor.description or []]
        rows = cursor.fetchmany(limit + 1)
        return {"columns": columns, "rows": [[cell(value) for value in row] for row in rows[:limit]], "truncated": len(rows) > limit}

    def browse(self, table, limit=50, offset=0, sort=None, direction="desc", search=""):
        with self.connect() as connection:
            if table not in self.tables(connection):
                raise ValueError("Unknown table")
            names = [column["name"] for column in self.columns(connection, table)]
            sort = sort or next((name for name in ("observed_at", "created_at", "id", "slot") if name in names), names[0])
            if sort not in names or direction not in ("asc", "desc"):
                raise ValueError("Invalid sort column or direction")
            where, parameters = "", []
            if search:
                searchable = [name for name in names if name not in ("signed_transaction", "raw_payload", "simulation_json")]
                where = " WHERE " + " OR ".join(f"CAST({identifier(name)} AS TEXT) LIKE ? ESCAPE '\\'" for name in searchable)
                term = "%" + search.replace("\\", "\\\\").replace("%", "\\%").replace("_", "\\_") + "%"
                parameters = [term] * len(searchable)
            count = connection.execute(f"SELECT count(*) FROM {identifier(table)}{where}", parameters).fetchone()[0]
            cursor = connection.execute(f"SELECT * FROM {identifier(table)}{where} ORDER BY {identifier(sort)} {direction} LIMIT ? OFFSET ?", parameters + [limit, offset])
            return {**self.result(cursor, limit), "total": count, "offset": offset, "sort": sort, "direction": direction}

    def journal(self, limit=50, offset=0, target="all", status="all", search=""):
        with self.connect() as connection:
            copies = {c["name"] for c in self.columns(connection, "copy_attempts")}
            sources = {c["name"] for c in self.columns(connection, "source_transactions")}
            slot = "a.landed_slot" if "landed_slot" in copies else "NULL"
            timings = "s.timings_json" if "timings_json" in sources else "NULL"
            filters, parameters = [], []
            if target != "all":
                filters.append("a.execution_target = ?")
                parameters.append(target)
            if status != "all":
                filters.append("a.status = ?")
                parameters.append(status)
            if search:
                filters.append("(instr(s.signature,?) > 0 OR instr(COALESCE(a.local_signature,''),?) > 0 OR instr(COALESCE(s.pool,''),?) > 0)")
                parameters.extend([search] * 3)
            where = " WHERE " + " AND ".join(filters) if filters else ""
            source = " FROM copy_attempts a JOIN source_transactions s ON s.signature=a.source_signature"
            count = connection.execute("SELECT count(*)" + source + where, parameters).fetchone()[0]
            sql = f"""SELECT a.id, s.signature AS source_signature, a.status, a.execution_target AS target,
                s.dex, s.slot AS source_slot, {slot} AS copy_slot,
                CASE WHEN a.execution_target='mainnet' THEN {slot}-s.slot ELSE NULL END AS slot_delta,
                a.route_latency_ms, a.created_at, a.local_signature, {timings} AS timings_json
                {source}{where} ORDER BY a.created_at DESC,a.id DESC LIMIT ? OFFSET ?"""
            return {**self.result(connection.execute(sql, parameters + [limit, offset]), limit),
                    "total": count, "offset": offset, "has_landed_slots": "landed_slot" in copies}

    def trade(self, signature):
        with self.connect() as connection:
            source = self.result(connection.execute("SELECT * FROM source_transactions WHERE signature=?", (signature,)), 1)
            if not source["rows"]:
                raise ValueError("Trade not found")
            copy = self.result(connection.execute("SELECT * FROM copy_attempts WHERE source_signature=?", (signature,)), 1)
            return {"source": dict(zip(source["columns"], source["rows"][0])),
                    "copy": dict(zip(copy["columns"], copy["rows"][0])) if copy["rows"] else None}

    @staticmethod
    def authorize(action, first, second, database, source):
        if action in (sqlite3.SQLITE_SELECT, sqlite3.SQLITE_RECURSIVE):
            return sqlite3.SQLITE_OK
        if action == sqlite3.SQLITE_READ and database in ("main", None):
            return sqlite3.SQLITE_OK
        if action == sqlite3.SQLITE_FUNCTION and (second or "").lower() not in ("load_extension", "readfile", "writefile"):
            return sqlite3.SQLITE_OK
        if action == sqlite3.SQLITE_PRAGMA and (first or "").lower() in ("table_info", "table_xinfo", "index_info", "index_list", "foreign_key_list"):
            return sqlite3.SQLITE_OK
        return sqlite3.SQLITE_DENY

    def query(self, sql):
        if not isinstance(sql, str) or not sql.strip() or len(sql) > 50000:
            raise ValueError("Enter a SQL query (up to 50,000 characters)")
        started = time.perf_counter()
        with self.connect() as connection:
            connection.set_authorizer(self.authorize)
            result = self.result(connection.execute(sql))
            return {**result, "elapsed_ms": round((time.perf_counter() - started) * 1000, 2)}


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def send_data(self, status, body, content_type="application/json"):
        data = json.dumps(body, allow_nan=False).encode() if content_type == "application/json" else body
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Cache-Control", "no-store")
        self.send_header("X-Content-Type-Options", "nosniff")
        self.send_header("Content-Security-Policy", "default-src 'self'; style-src 'self'; script-src 'self'; connect-src 'self'; img-src 'self' data:; frame-ancestors 'none'; base-uri 'none'; form-action 'self'")
        self.end_headers()
        self.wfile.write(data)

    def local_request(self):
        hosts = {f"127.0.0.1:{self.server.server_port}", f"localhost:{self.server.server_port}"}
        if self.headers.get("Host") not in hosts:
            return False
        if self.headers.get("Sec-Fetch-Site") == "cross-site":
            # Opening the empty UI shell from a link is safe; API data still requires same-origin requests.
            if not (self.command == "GET" and urlparse(self.path).path == "/" and self.headers.get("Sec-Fetch-Mode") == "navigate"):
                return False
        origin = self.headers.get("Origin")
        return not origin or origin in {"http://" + host for host in hosts}

    def handle_request(self):
        if not self.local_request():
            self.send_data(403, {"error": "Open this workbench from its localhost address"})
            return
        route = urlparse(self.path)
        params = {key: values[0] for key, values in parse_qs(route.query).items()}
        database = self.server.database
        try:
            if self.command == "POST" and route.path == "/api/query":
                size = int(self.headers.get("Content-Length", "0"))
                if not 0 < size <= 100000:
                    raise ValueError("Invalid query request size")
                request = json.loads(self.rfile.read(size))
                result = database.query(request.get("sql"))
            elif self.command != "GET":
                self.send_data(404, {"error": "Not found"})
                return
            elif route.path == "/api/catalog":
                result = database.catalog()
            elif route.path in ("/api/journal", "/api/table"):
                limit, offset = int(params.get("limit", "50")), int(params.get("offset", "0"))
                if not 1 <= limit <= MAX_ROWS or offset < 0:
                    raise ValueError("Invalid page size or offset")
                if route.path == "/api/journal":
                    result = database.journal(limit, offset, params.get("target", "all"), params.get("status", "all"), params.get("search", "")[:200])
                else:
                    result = database.browse(params.get("name", ""), limit, offset, params.get("sort"), params.get("direction", "desc"), params.get("search", "")[:200])
            elif route.path == "/api/trade":
                result = database.trade(params.get("signature", ""))
            elif route.path in ("/", "/app.js", "/styles.css"):
                filename, mime = {"/": ("index.html", "text/html; charset=utf-8"), "/app.js": ("app.js", "text/javascript; charset=utf-8"), "/styles.css": ("styles.css", "text/css; charset=utf-8")}[route.path]
                self.send_data(200, (ROOT / "static" / filename).read_bytes(), mime)
                return
            else:
                self.send_data(404, {"error": "Not found"})
                return
            self.send_data(200, result)
        except (ValueError, TypeError, AttributeError, sqlite3.Error) as error:
            message = str(error)
            if "interrupted" in message:
                message = "Query exceeded the 2-second time limit. Narrow the query and try again."
            elif "not authorized" in message or "readonly" in message:
                message = "Read-only SQL only. Changes, attached databases, and unsafe operations are disabled."
            self.send_data(400, {"error": message})

    do_GET = handle_request
    do_POST = handle_request


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--db", type=Path, default=ROOT.parent / "copy_trader.sqlite")
    parser.add_argument("--port", type=int, default=8765)
    args = parser.parse_args()
    try:
        database = Database(args.db)
        database.catalog()
        server = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    except (OSError, ValueError, sqlite3.Error) as error:
        parser.exit(1, f"Cannot start database workbench: {error}\n")
    server.database = database
    print(f"Database workbench: http://127.0.0.1:{server.server_port}", flush=True)
    print(f"Read-only: {database.path}", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()


if __name__ == "__main__":
    main()
