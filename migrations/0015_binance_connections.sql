CREATE TABLE "binance_connections" (
	"id" serial PRIMARY KEY NOT NULL,
	"account_id" text NOT NULL,
	"agent_id" text,
	"access_token_encrypted" text NOT NULL,
	"refresh_token_encrypted" text,
	"scope" text,
	"expires_at" timestamp with time zone,
	"tools" jsonb DEFAULT '[]'::jsonb NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"updated_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE UNIQUE INDEX "binance_connections_owner_idx" ON "binance_connections" USING btree ("account_id",coalesce("agent_id", ''));--> statement-breakpoint
CREATE TABLE "binance_oauth_states" (
	"state" text PRIMARY KEY NOT NULL,
	"account_id" text NOT NULL,
	"agent_id" text,
	"verifier_encrypted" text NOT NULL,
	"return_to" text,
	"expires_at" timestamp with time zone NOT NULL
);
--> statement-breakpoint
CREATE TABLE "binance_agent_orders" (
	"id" serial PRIMARY KEY NOT NULL,
	"agent_id" text NOT NULL,
	"tool" text NOT NULL,
	"symbol" text,
	"notional_usd" double precision NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE INDEX "binance_agent_orders_agent_idx" ON "binance_agent_orders" USING btree ("agent_id","created_at");
