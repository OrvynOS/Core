CREATE TABLE "credit_balances" (
	"account_id" text PRIMARY KEY NOT NULL,
	"micro_usd" bigint DEFAULT 0 NOT NULL,
	"updated_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE TABLE "credit_ledger" (
	"id" serial PRIMARY KEY NOT NULL,
	"account_id" text NOT NULL,
	"kind" text NOT NULL,
	"micro_usd" bigint NOT NULL,
	"tx_hash" text,
	"token" text,
	"amount" text,
	"model" text,
	"source" text,
	"input_tokens" integer,
	"output_tokens" integer,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE UNIQUE INDEX "credit_ledger_tx_idx" ON "credit_ledger" USING btree ("tx_hash") WHERE "tx_hash" IS NOT NULL;--> statement-breakpoint
CREATE INDEX "credit_ledger_account_idx" ON "credit_ledger" USING btree ("account_id","created_at");--> statement-breakpoint
CREATE TABLE "inference_keys" (
	"id" text PRIMARY KEY NOT NULL,
	"account_id" text NOT NULL,
	"name" text NOT NULL,
	"key_hash" text NOT NULL,
	"hint" text NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"last_used_at" timestamp with time zone,
	"revoked_at" timestamp with time zone
);
--> statement-breakpoint
CREATE UNIQUE INDEX "inference_keys_hash_idx" ON "inference_keys" USING btree ("key_hash");--> statement-breakpoint
CREATE INDEX "inference_keys_account_idx" ON "inference_keys" USING btree ("account_id");--> statement-breakpoint
CREATE TABLE "chat_turn_threads" (
	"id" text PRIMARY KEY NOT NULL,
	"account_id" text NOT NULL,
	"title" text NOT NULL,
	"model" text NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"updated_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE INDEX "chat_turn_threads_account_idx" ON "chat_turn_threads" USING btree ("account_id","updated_at");--> statement-breakpoint
CREATE TABLE "chat_turn_messages" (
	"id" serial PRIMARY KEY NOT NULL,
	"thread_id" text NOT NULL,
	"role" text NOT NULL,
	"content" text NOT NULL,
	"model" text,
	"micro_usd" bigint DEFAULT 0 NOT NULL,
	"input_tokens" integer,
	"output_tokens" integer,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE INDEX "chat_turn_messages_thread_idx" ON "chat_turn_messages" USING btree ("thread_id","id");
