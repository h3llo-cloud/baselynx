-- Test data for baselynx. Loaded into a template database by the test harness.

CREATE SCHEMA demo;

CREATE TABLE demo.customers (
    id          serial PRIMARY KEY,
    name        text NOT NULL,
    balance     numeric(12,2) NOT NULL DEFAULT 0,
    active      boolean NOT NULL DEFAULT true,
    created_at  timestamptz NOT NULL DEFAULT '2026-01-01 00:00:00+00',
    note        text,
    tier        text NOT NULL DEFAULT 'basic'
);
INSERT INTO demo.customers (name, balance, active, created_at, note)
SELECT 'Customer ' || g,
       round((g * 13.37)::numeric % 10000, 2),
       g % 7 <> 0,
       timestamptz '2026-01-01 00:00:00+00' + g * interval '1 hour',
       CASE WHEN g % 3 = 0 THEN NULL ELSE 'note ' || g END
FROM generate_series(1, 1000) AS g;

CREATE TABLE demo.events (
    id          bigint PRIMARY KEY,
    customer_id integer NOT NULL,
    kind        text NOT NULL,
    amount      numeric(12,2),
    happened_at timestamptz NOT NULL,
    payload     jsonb,
    processed   boolean NOT NULL,
    source      text
);
INSERT INTO demo.events
SELECT g,
       1 + g % 900,
       (ARRAY['login', 'purchase', 'refund', 'logout'])[1 + g % 4],
       CASE WHEN g % 5 = 0 THEN NULL ELSE (g % 997)::numeric / 10 END,
       timestamptz '2025-01-01 00:00:00+00' + g * interval '30 seconds',
       jsonb_build_object('n', g, 'even', g % 2 = 0),
       g % 2 = 0,
       CASE g % 3 WHEN 0 THEN 'web' WHEN 1 THEN 'mobile' ELSE 'api' END
FROM generate_series(1, 1000000) AS g;

-- Customers above 900 have no events, so tests may change or delete them.
ALTER TABLE demo.events ADD CONSTRAINT events_customer_id_fkey
    FOREIGN KEY (customer_id) REFERENCES demo.customers (id);
CREATE INDEX events_customer_id ON demo.events (customer_id);

CREATE TABLE demo.composite_key (
    region  text NOT NULL,
    code    integer NOT NULL,
    label   text,
    PRIMARY KEY (region, code)
);
INSERT INTO demo.composite_key VALUES ('eu', 1, 'one'), ('eu', 2, 'two'), ('us', 1, 'uno');

-- A two-column foreign key; only ('eu', 1) is referenced.
CREATE TABLE demo.composite_ref (
    id      integer PRIMARY KEY,
    region  text,
    code    integer,
    FOREIGN KEY (region, code) REFERENCES demo.composite_key (region, code)
);
INSERT INTO demo.composite_ref VALUES (1, 'eu', 1), (2, NULL, NULL);

CREATE TABLE demo.no_pk (
    a integer,
    b text
);
INSERT INTO demo.no_pk VALUES (1, 'x'), (1, 'x'), (2, 'y');

CREATE TABLE demo.unique_only (
    email text NOT NULL,
    name  text
);
CREATE UNIQUE INDEX unique_only_email ON demo.unique_only (email);
INSERT INTO demo.unique_only VALUES ('a@example.com', 'A'), ('b@example.com', 'B');

CREATE TYPE demo.mood AS ENUM ('sad', 'ok', 'happy');

CREATE TABLE demo.all_types (
    id          integer PRIMARY KEY,
    c_int2      int2,
    c_int4      int4,
    c_int8      int8,
    c_numeric   numeric,
    c_float8    float8,
    c_bool      bool,
    c_text      text,
    c_varchar   varchar(20),
    c_bytea     bytea,
    c_date      date,
    c_time      time,
    c_timestamp timestamp,
    c_timestamptz timestamptz,
    c_interval  interval,
    c_uuid      uuid,
    c_json      json,
    c_jsonb     jsonb,
    c_int4_arr  int4[],
    c_text_arr  text[],
    c_inet      inet,
    c_mood      demo.mood,
    c_generated integer GENERATED ALWAYS AS (id * 10) STORED,
    c_identity  integer GENERATED ALWAYS AS IDENTITY
);
INSERT INTO demo.all_types (id, c_int2, c_int4, c_int8, c_numeric, c_float8, c_bool, c_text, c_varchar, c_bytea,
    c_date, c_time, c_timestamp, c_timestamptz, c_interval, c_uuid, c_json, c_jsonb, c_int4_arr, c_text_arr, c_inet, c_mood)
VALUES
 (1, 12, 123456, 9007199254740993, 12345.6789, 3.5, true, 'hello, "world"', 'short', '\x0102ff',
  '2026-11-11', '13:45:00', '2026-11-11 13:45:00', '2026-11-11 13:45:00+00', '1 day 02:03:04',
  '6f1c2b8e-1c55-4b0e-9d43-3f1a2b3c4d5e', '{"a": 1}', '{"b": [1, 2]}', '{1,2,3}', '{"x","y z"}', '192.168.0.1', 'happy'),
 (2, -1, 0, 42, 0.000123, -1e10, false, E'line one\nline two', 'émoji ✓', '\x', '1999-12-31', '00:00:01',
  '1999-12-31 23:59:59', '1999-12-31 23:59:59+00', '-3 hours', '00000000-0000-0000-0000-000000000000',
  '[]', '{}', '{}', '{}', '::1', 'sad'),
 (3, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL);

CREATE TABLE demo."Weird Name" (
    id        integer PRIMARY KEY,
    "select"  text,
    "a""b"    text
);
INSERT INTO demo."Weird Name" VALUES (1, 'from', 'quoted'), (2, 'where', NULL);

CREATE VIEW demo.customers_v AS SELECT id, name, balance FROM demo.customers;

ANALYZE;
