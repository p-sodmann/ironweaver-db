-- The event table of docker/projection.example.toml (compose profile "postgres")
CREATE TABLE order_events (
    id bigserial PRIMARY KEY,
    kind text NOT NULL,
    payload jsonb NOT NULL
);
