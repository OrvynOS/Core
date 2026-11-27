CREATE TABLE "binance_wallet_sessions" (
	"agent_id" text PRIMARY KEY NOT NULL,
	"account_id" text NOT NULL,
	"instance_encrypted" text NOT NULL,
	"session_encrypted" text,
	"status" text DEFAULT 'disconnected' NOT NULL,
	"qr_code_id" text,
	"pairing_code" text,
	"signin_url" text,
	"signin_expires_at" timestamp with time zone,
	"addresses" jsonb DEFAULT '[]'::jsonb NOT NULL,
	"settings" jsonb,
	"error" text,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"updated_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE TABLE "binance_wallet_orders" (
	"id" serial PRIMARY KEY NOT NULL,
	"agent_id" text NOT NULL,
	"kind" text NOT NULL,
	"notional_usd" double precision NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE INDEX "binance_wallet_orders_agent_idx" ON "binance_wallet_orders" USING btree ("agent_id","created_at");
