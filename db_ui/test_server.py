import json
from pathlib import Path
import sqlite3
import tempfile
import threading
import unittest
from urllib.request import Request, urlopen
from urllib.error import HTTPError

from server import Database, Handler, ThreadingHTTPServer


class DatabaseTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.path = Path(self.directory.name) / 'journal.sqlite'
        with sqlite3.connect(self.path) as connection:
            connection.executescript('''
                CREATE TABLE source_transactions(signature TEXT PRIMARY KEY, slot INTEGER, dex TEXT, pool TEXT, observed_at INTEGER, timings_json TEXT);
                CREATE TABLE copy_attempts(id INTEGER PRIMARY KEY, source_signature TEXT, local_signature TEXT, status TEXT, execution_target TEXT, route_latency_ms INTEGER, created_at INTEGER, signed_transaction BLOB);
                INSERT INTO source_transactions VALUES ('abc',100,'pump_swap','pool',1000,'{}');
                INSERT INTO copy_attempts VALUES (1,'abc','def','landed','mainnet',502,1000,X'0102');
            ''')
        self.database = Database(self.path)

    def tearDown(self):
        self.directory.cleanup()

    def test_old_schema_browsing_and_details(self):
        catalog = self.database.catalog()
        self.assertEqual(len(catalog['tables']),2)
        result = self.database.journal()
        self.assertEqual(result['total'],1)
        self.assertFalse(result['has_landed_slots'])
        self.assertIsNone(result['rows'][0][result['columns'].index('copy_slot')])
        self.assertEqual(self.database.journal(target='surfpool')['total'],0)
        self.assertEqual(self.database.journal(status='failed')['total'],0)
        self.assertEqual(self.database.journal(search='abc')['total'],1)
        self.assertEqual(self.database.journal(search="' OR 1=1 --")['total'],0)
        self.assertEqual(self.database.browse('copy_attempts',search='landed')['total'],1)
        self.assertEqual(self.database.browse('copy_attempts',search='%')['total'],0)
        self.assertEqual(self.database.browse('copy_attempts',offset=1)['rows'],[])
        self.assertEqual(self.database.trade('abc')['copy']['signed_transaction'],'[binary: 2 bytes]')
        with self.assertRaises(ValueError):
            self.database.browse('copy_attempts; DROP TABLE copy_attempts')
        with self.assertRaises(ValueError):
            self.database.browse('copy_attempts',sort='no_such_column')

    def test_landing_slot_support(self):
        with sqlite3.connect(self.path) as connection:
            connection.execute('ALTER TABLE copy_attempts ADD COLUMN landed_slot INTEGER')
            connection.execute('UPDATE copy_attempts SET landed_slot=103')
        result = self.database.journal()
        self.assertTrue(result['has_landed_slots'])
        self.assertEqual(result['rows'][0][result['columns'].index('slot_delta')],3)

    def test_read_only_queries_and_no_external_database_access(self):
        self.assertEqual(self.database.query('SELECT count(*) AS n FROM copy_attempts')['rows'],[[1]])
        self.assertEqual(self.database.query('WITH x AS (SELECT 7 AS value) SELECT * FROM x')['rows'],[[7]])
        self.assertTrue(self.database.query("PRAGMA table_info('copy_attempts')")['rows'])
        statements = [
            "DELETE FROM copy_attempts", "UPDATE copy_attempts SET status='failed'",
            "INSERT INTO copy_attempts(id) VALUES(2)", "DROP TABLE copy_attempts",
            "CREATE TABLE bad(x)", "PRAGMA query_only=OFF", "PRAGMA user_version=5",
            "ATTACH DATABASE ':memory:' AS other", "VACUUM INTO '/tmp/workbench-should-not-exist.sqlite'",
            "SELECT load_extension('anything')", "SELECT 1; SELECT 2",
        ]
        for statement in statements:
            with self.subTest(statement=statement), self.assertRaises(sqlite3.Error):
                self.database.query(statement)
        self.assertEqual(self.database.query('SELECT status FROM copy_attempts')['rows'],[['landed']])

    def test_limits_and_exact_integers(self):
        result = self.database.query('WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<600) SELECT x FROM n')
        self.assertEqual(len(result['rows']),500)
        self.assertTrue(result['truncated'])
        self.assertEqual(self.database.query('SELECT 9223372036854775807')['rows'],[['9223372036854775807']])
        with self.assertRaises(sqlite3.Error):
            self.database.query('SELECT zeroblob(2000000)')

    def test_http_serves_ui_and_rejects_foreign_origins(self):
        server = ThreadingHTTPServer(('127.0.0.1',0),Handler)
        server.database = self.database
        thread = threading.Thread(target=server.serve_forever,daemon=True)
        thread.start()
        base = f'http://127.0.0.1:{server.server_port}'
        try:
            with urlopen(base+'/') as response:
                self.assertIn(b'Database workbench',response.read())
                self.assertIn("frame-ancestors 'none'",response.headers['Content-Security-Policy'])
            with urlopen(Request(base+'/',headers={'Sec-Fetch-Site':'cross-site','Sec-Fetch-Mode':'navigate'})) as response:
                self.assertEqual(response.status,200)
            request = Request(base+'/api/query',data=json.dumps({'sql':'SELECT count(*) FROM copy_attempts'}).encode(),headers={'Content-Type':'application/json','Origin':base})
            with urlopen(request) as response:
                self.assertEqual(json.load(response)['rows'],[[1]])
            for headers in [{'Origin':'https://example.com'},{'Host':'evil.example'},{'Sec-Fetch-Site':'cross-site'}]:
                with self.subTest(headers=headers), self.assertRaises(HTTPError) as error:
                    urlopen(Request(base+'/api/catalog',headers=headers))
                self.assertEqual(error.exception.code,403)
            request = Request(base+'/api/query',data=json.dumps({'sql':'DELETE FROM copy_attempts'}).encode(),headers={'Content-Type':'application/json'})
            with self.assertRaises(HTTPError) as error:
                urlopen(request)
            self.assertEqual(error.exception.code,400)
            self.assertEqual(self.database.query('SELECT count(*) FROM copy_attempts')['rows'],[[1]])
        finally:
            server.shutdown()
            server.server_close()
            thread.join()


if __name__ == '__main__':
    unittest.main()
