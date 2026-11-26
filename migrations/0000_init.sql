CREATE TABLE "agent_cards" (
	"agent_id" text PRIMARY KEY NOT NULL,
	"owner_account_id" text NOT NULL,
	"card_id" text NOT NULL,
	"environment" text NOT NULL,
	"chain_id" text NOT NULL,
	"asset" text NOT NULL,
	"enabled" boolean DEFAULT false NOT NULL,
	"per_purchase_limit_usd" double precision DEFAULT 50 NOT NULL,
	"monthly_limit_usd" double precision DEFAULT 200 NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE TABLE "agent_logs" (
	"id" serial PRIMARY KEY NOT NULL,
	"agent_id" text NOT NULL,
	"kind" text NOT NULL,
	"message" text NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE TABLE "agent_plugins" (
	"agent_id" text NOT NULL,
	"plugin_id" text NOT NULL,
	"config" jsonb NOT NULL,
	"state" jsonb DEFAULT '{}'::jsonb NOT NULL,
	"enabled" boolean DEFAULT true NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "agent_plugins_agent_id_plugin_id_pk" PRIMARY KEY("agent_id","plugin_id")
);
--> statement-breakpoint
CREATE TABLE "agents" (
	"id" text PRIMARY KEY NOT NULL,
	"handle" text,
	"name" text NOT NULL,
	"symbol" text NOT NULL,
	"description" text DEFAULT '' NOT NULL,
	"icon" text,
	"links" jsonb,
	"owner_account_id" text NOT NULL,
	"persona" text DEFAULT '' NOT NULL,
	"strategy" text DEFAULT '' NOT NULL,
	"autopilot_enabled" boolean DEFAULT false NOT NULL,
	"autopilot_interval_min" integer DEFAULT 60 NOT NULL,
	"billing_mode" text DEFAULT 'manual' NOT NULL,
	"registered" boolean DEFAULT false NOT NULL,
	"paid_until" timestamp with time zone,
	"subscription_active" boolean DEFAULT true NOT NULL,
	"hosting_status" text DEFAULT 'none' NOT NULL,
	"held_reason" text,
	"render_service_id" text,
	"host_url" text,
	"host_internal" text,
	"pending_treasury_months" integer DEFAULT 0 NOT NULL,
	"rate_limit_per_minute" integer DEFAULT 60 NOT NULL,
	"agent_id_name" text,
	"agent_id_verified_at" timestamp with time zone,
	"erc8004_agent_id" text,
	"erc8004_error" text,
	"billing_details" text DEFAULT '' NOT NULL,
	"tax" jsonb,
	"tax_allocated_wei" text DEFAULT '0' NOT NULL,
	"orvyn_fee_owed_wei" text DEFAULT '0' NOT NULL,
	"min_holding_tokens" double precision DEFAULT 1 NOT NULL,
	"chat_messages_per_hour" integer DEFAULT 20 NOT NULL,
	"gas_reserve_bnb" double precision DEFAULT 0.01 NOT NULL,
	"ai_provider" text NOT NULL,
	"ai_model" text NOT NULL,
	"ai_key_encrypted" text NOT NULL,
	"ai_key_hint" text NOT NULL,
	"evm_address" text NOT NULL,
	"evm_wallet" jsonb NOT NULL,
	"status" text DEFAULT 'awaiting_funds' NOT NULL,
	"status_detail" text,
	"launchpad" text DEFAULT 'fourmeme' NOT NULL,
	"launch_request_id" text,
	"token_address" text,
	"launch_tx" text,
	"graduated" boolean DEFAULT false NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "agents_handle_unique" UNIQUE("handle"),
	CONSTRAINT "agents_agent_id_name_unique" UNIQUE("agent_id_name")
);
--> statement-breakpoint
CREATE TABLE "binance_pay_orders" (
	"id" text PRIMARY KEY NOT NULL,
	"agent_id" text NOT NULL,
	"months" integer NOT NULL,
	"amount_usd" text NOT NULL,
	"status" text DEFAULT 'pending' NOT NULL,
	"prepay_id" text,
	"checkout_url" text,
	"created_by" text,
	"tx_hash" text,
	"error" text,
	"expires_at" timestamp with time zone NOT NULL,
	"paid_at" timestamp with time zone,
	"credited_at" timestamp with time zone,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE TABLE "chat_messages" (
	"id" serial PRIMARY KEY NOT NULL,
	"agent_id" text NOT NULL,
	"account_id" text NOT NULL,
	"role" text NOT NULL,
	"content" text NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE TABLE "external_agent_keys" (
	"key_hash" text PRIMARY KEY NOT NULL,
	"agent_id" text NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"revoked_at" timestamp with time zone
);
--> statement-breakpoint
CREATE TABLE "external_plugins" (
	"slug" text PRIMARY KEY NOT NULL,
	"submitted_by" text NOT NULL,
	"live" jsonb,
	"submission" jsonb,
	"review_note" text,
	"reviewed_by" text,
	"reviewed_at" timestamp with time zone,
	"suspended" boolean DEFAULT false NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"updated_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE TABLE "fee_claims" (
	"id" serial PRIMARY KEY NOT NULL,
	"agent_id" text NOT NULL,
	"kind" text NOT NULL,
	"amount" text,
	"tx" text,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE TABLE "hire_messages" (
	"id" serial PRIMARY KEY NOT NULL,
	"hire_id" text NOT NULL,
	"author" text NOT NULL,
	"text" text NOT NULL,
	"via_email" boolean DEFAULT false NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE TABLE "hires" (
	"id" text PRIMARY KEY NOT NULL,
	"agent_id" text NOT NULL,
	"client_account_id" text NOT NULL,
	"client_email" text,
	"offer_title" text NOT NULL,
	"brief" text NOT NULL,
	"token" text NOT NULL,
	"amount" text NOT NULL,
	"fee_bps" integer NOT NULL,
	"status" text DEFAULT 'awaiting_payment' NOT NULL,
	"deadline_at" timestamp with time zone NOT NULL,
	"funded_at" timestamp with time zone,
	"delivered_at" timestamp with time zone,
	"review_ends_at" timestamp with time zone,
	"settled_at" timestamp with time zone,
	"result" text,
	"result_hash" text,
	"needs_agent" boolean DEFAULT false NOT NULL,
	"agent_attempts" integer DEFAULT 0 NOT NULL,
	"chain_action" text,
	"chain_error" text,
	"paid_to_agent" text,
	"refunded" text,
	"fee" text,
	"rating" integer,
	"review" text,
	"email_thread_id" text,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"updated_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE TABLE "invoices" (
	"id" serial PRIMARY KEY NOT NULL,
	"agent_id" text NOT NULL,
	"payment_index" integer NOT NULL,
	"payer" text NOT NULL,
	"token" text NOT NULL,
	"symbol" text NOT NULL,
	"amount" text NOT NULL,
	"decimals" integer NOT NULL,
	"months" integer NOT NULL,
	"period_start" timestamp with time zone NOT NULL,
	"period_end" timestamp with time zone NOT NULL,
	"paid_at" timestamp with time zone NOT NULL,
	"block_number" text NOT NULL,
	"tx_hash" text,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE TABLE "jobs" (
	"agent_id" text NOT NULL,
	"task" text NOT NULL,
	"next_run_at" timestamp with time zone DEFAULT now() NOT NULL,
	"locked_until" timestamp with time zone,
	"last_error" text,
	CONSTRAINT "jobs_agent_id_task_pk" PRIMARY KEY("agent_id","task")
);
--> statement-breakpoint
CREATE TABLE "knowledge_docs" (
	"id" serial PRIMARY KEY NOT NULL,
	"agent_id" text NOT NULL,
	"title" text NOT NULL,
	"content" text NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE TABLE "mail_messages" (
	"id" serial PRIMARY KEY NOT NULL,
	"agent_id" text NOT NULL,
	"direction" text NOT NULL,
	"status" text NOT NULL,
	"author" text,
	"message_id" text NOT NULL,
	"in_reply_to" text,
	"references" jsonb DEFAULT '[]'::jsonb NOT NULL,
	"thread_id" text NOT NULL,
	"from_address" text NOT NULL,
	"from_name" text DEFAULT '' NOT NULL,
	"to_addresses" jsonb NOT NULL,
	"cc_addresses" jsonb DEFAULT '[]'::jsonb NOT NULL,
	"subject" text DEFAULT '' NOT NULL,
	"text" text DEFAULT '' NOT NULL,
	"attachments" jsonb DEFAULT '[]'::jsonb NOT NULL,
	"auth" jsonb,
	"auto_submitted" boolean DEFAULT false NOT NULL,
	"read" boolean DEFAULT false NOT NULL,
	"handled" boolean DEFAULT false NOT NULL,
	"delivery" jsonb DEFAULT '{}'::jsonb NOT NULL,
	"attempts" integer DEFAULT 0 NOT NULL,
	"next_attempt_at" timestamp with time zone,
	"locked_until" timestamp with time zone,
	"last_error" text,
	"sent_at" timestamp with time zone,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE TABLE "mail_settings" (
	"agent_id" text PRIMARY KEY NOT NULL,
	"enabled" boolean DEFAULT true NOT NULL,
	"display_name" text,
	"blocked" jsonb DEFAULT '[]'::jsonb NOT NULL,
	"forward_to_owner" boolean DEFAULT false NOT NULL,
	"updated_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE TABLE "oauth_clients" (
	"id" text PRIMARY KEY NOT NULL,
	"name" text NOT NULL,
	"owner_account_id" text NOT NULL,
	"type" text NOT NULL,
	"secret_hash" text,
	"redirect_uris" jsonb NOT NULL,
	"scopes" jsonb NOT NULL,
	"homepage" text,
	"logo_url" text,
	"first_party" boolean DEFAULT false NOT NULL,
	"status" text DEFAULT 'active' NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE TABLE "oauth_codes" (
	"code_hash" text PRIMARY KEY NOT NULL,
	"client_id" text NOT NULL,
	"grant_id" integer NOT NULL,
	"subject" text NOT NULL,
	"scope" text NOT NULL,
	"redirect_uri" text NOT NULL,
	"code_challenge" text NOT NULL,
	"nonce" text,
	"expires_at" timestamp with time zone NOT NULL,
	"used_at" timestamp with time zone
);
--> statement-breakpoint
CREATE TABLE "oauth_device_codes" (
	"device_code_hash" text PRIMARY KEY NOT NULL,
	"user_code" text NOT NULL,
	"client_id" text NOT NULL,
	"scope" text NOT NULL,
	"status" text DEFAULT 'pending' NOT NULL,
	"grant_id" integer,
	"subject" text,
	"expires_at" timestamp with time zone NOT NULL,
	"last_polled_at" timestamp with time zone,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "oauth_device_codes_user_code_unique" UNIQUE("user_code")
);
--> statement-breakpoint
CREATE TABLE "oauth_grants" (
	"id" serial PRIMARY KEY NOT NULL,
	"client_id" text NOT NULL,
	"account_id" text NOT NULL,
	"subject" text NOT NULL,
	"scope" text NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"revoked_at" timestamp with time zone
);
--> statement-breakpoint
CREATE TABLE "oauth_refresh_tokens" (
	"id" serial PRIMARY KEY NOT NULL,
	"token_hash" text NOT NULL,
	"family_id" text NOT NULL,
	"client_id" text NOT NULL,
	"grant_id" integer NOT NULL,
	"subject" text NOT NULL,
	"scope" text NOT NULL,
	"expires_at" timestamp with time zone NOT NULL,
	"used_at" timestamp with time zone,
	"revoked_at" timestamp with time zone,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE TABLE "push_devices" (
	"token" text PRIMARY KEY NOT NULL,
	"account_id" text NOT NULL,
	"platform" text NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"last_seen_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE TABLE "shielded_keys" (
	"agent_id" text PRIMARY KEY NOT NULL,
	"public_key" text NOT NULL,
	"encryption_public" text NOT NULL,
	"sealed" text NOT NULL,
	"registered_tx" text,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE TABLE "trades" (
	"id" serial PRIMARY KEY NOT NULL,
	"agent_id" text NOT NULL,
	"plugin_id" text NOT NULL,
	"venue" text NOT NULL,
	"symbol" text NOT NULL,
	"side" text NOT NULL,
	"size" double precision NOT NULL,
	"size_unit" text NOT NULL,
	"leverage" integer,
	"price" double precision,
	"rationale" text NOT NULL,
	"result" jsonb,
	"ok" boolean NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE TABLE "trigger_orders" (
	"id" serial PRIMARY KEY NOT NULL,
	"agent_id" text NOT NULL,
	"plugin_id" text NOT NULL,
	"market" text NOT NULL,
	"kind" text NOT NULL,
	"trigger_price" double precision NOT NULL,
	"price_unit" text NOT NULL,
	"sell_percent" integer DEFAULT 100 NOT NULL,
	"source" text NOT NULL,
	"status" text DEFAULT 'open' NOT NULL,
	"attempts" integer DEFAULT 0 NOT NULL,
	"note" text,
	"result" jsonb,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"triggered_at" timestamp with time zone
);
--> statement-breakpoint
CREATE TABLE "used_nonces" (
	"nonce" text PRIMARY KEY NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE TABLE "users" (
	"account_id" text PRIMARY KEY NOT NULL,
	"sign_in_method" text NOT NULL,
	"privy_user_id" text,
	"email" text,
	"social_provider" text,
	"social_username" text,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"last_sign_in_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "users_privy_user_id_unique" UNIQUE("privy_user_id")
);
--> statement-breakpoint
CREATE TABLE "vyn_messages" (
	"id" serial PRIMARY KEY NOT NULL,
	"thread_id" text NOT NULL,
	"message" jsonb NOT NULL,
	"device" text,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE TABLE "vyn_settings" (
	"account_id" text PRIMARY KEY NOT NULL,
	"ai_provider" text NOT NULL,
	"ai_model" text NOT NULL,
	"ai_key_encrypted" text NOT NULL,
	"ai_key_hint" text NOT NULL,
	"auto_approve_usd" double precision DEFAULT 0 NOT NULL,
	"updated_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE TABLE "vyn_threads" (
	"id" text PRIMARY KEY NOT NULL,
	"account_id" text NOT NULL,
	"title" text DEFAULT 'New conversation' NOT NULL,
	"busy_until" timestamp with time zone,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"updated_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE TABLE "wallet_grants_used" (
	"nonce" text PRIMARY KEY NOT NULL,
	"agent_id" text NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE TABLE "wallet_log" (
	"id" serial PRIMARY KEY NOT NULL,
	"agent_id" text NOT NULL,
	"scheme" text NOT NULL,
	"summary" text,
	"ok" boolean NOT NULL,
	"error" text,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE TABLE "wallet_spend" (
	"agent_id" text NOT NULL,
	"day" text NOT NULL,
	"asset" text NOT NULL,
	"amount" numeric(78, 0) NOT NULL,
	CONSTRAINT "wallet_spend_agent_id_day_asset_pk" PRIMARY KEY("agent_id","day","asset")
);
--> statement-breakpoint
CREATE TABLE "withdrawals" (
	"id" serial PRIMARY KEY NOT NULL,
	"agent_id" text NOT NULL,
	"requested_by" text NOT NULL,
	"asset" text NOT NULL,
	"symbol" text NOT NULL,
	"amount" text NOT NULL,
	"destination" text NOT NULL,
	"tx" text,
	"status" text NOT NULL,
	"error" text,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE INDEX "logs_agent_idx" ON "agent_logs" USING btree ("agent_id","created_at");--> statement-breakpoint
CREATE INDEX "binance_pay_orders_agent_idx" ON "binance_pay_orders" USING btree ("agent_id");--> statement-breakpoint
CREATE INDEX "binance_pay_orders_status_idx" ON "binance_pay_orders" USING btree ("status");--> statement-breakpoint
CREATE INDEX "chat_agent_account_idx" ON "chat_messages" USING btree ("agent_id","account_id","created_at");--> statement-breakpoint
CREATE INDEX "hire_messages_idx" ON "hire_messages" USING btree ("hire_id","id");--> statement-breakpoint
CREATE INDEX "hires_agent_idx" ON "hires" USING btree ("agent_id","status");--> statement-breakpoint
CREATE INDEX "hires_client_idx" ON "hires" USING btree ("client_account_id","created_at");--> statement-breakpoint
CREATE INDEX "hires_thread_idx" ON "hires" USING btree ("email_thread_id");--> statement-breakpoint
CREATE UNIQUE INDEX "invoices_payment_idx" ON "invoices" USING btree ("agent_id","payment_index");--> statement-breakpoint
CREATE INDEX "jobs_due_idx" ON "jobs" USING btree ("next_run_at");--> statement-breakpoint
CREATE INDEX "knowledge_agent_idx" ON "knowledge_docs" USING btree ("agent_id");--> statement-breakpoint
CREATE INDEX "mail_agent_idx" ON "mail_messages" USING btree ("agent_id","direction","created_at");--> statement-breakpoint
CREATE INDEX "mail_queue_idx" ON "mail_messages" USING btree ("status","next_attempt_at");--> statement-breakpoint
CREATE INDEX "mail_thread_idx" ON "mail_messages" USING btree ("agent_id","thread_id");--> statement-breakpoint
CREATE INDEX "mail_message_id_idx" ON "mail_messages" USING btree ("message_id");--> statement-breakpoint
CREATE UNIQUE INDEX "oauth_grants_unique" ON "oauth_grants" USING btree ("client_id","account_id","subject");--> statement-breakpoint
CREATE UNIQUE INDEX "oauth_refresh_hash_idx" ON "oauth_refresh_tokens" USING btree ("token_hash");--> statement-breakpoint
CREATE INDEX "oauth_refresh_family_idx" ON "oauth_refresh_tokens" USING btree ("family_id");--> statement-breakpoint
CREATE INDEX "push_devices_account_idx" ON "push_devices" USING btree ("account_id");--> statement-breakpoint
CREATE INDEX "trades_agent_idx" ON "trades" USING btree ("agent_id","created_at");--> statement-breakpoint
CREATE INDEX "triggers_open_idx" ON "trigger_orders" USING btree ("agent_id","plugin_id","status");--> statement-breakpoint
CREATE INDEX "vyn_messages_thread_idx" ON "vyn_messages" USING btree ("thread_id","id");--> statement-breakpoint
CREATE INDEX "vyn_threads_account_idx" ON "vyn_threads" USING btree ("account_id","updated_at");--> statement-breakpoint
CREATE INDEX "wallet_log_agent_idx" ON "wallet_log" USING btree ("agent_id","created_at");--> statement-breakpoint
CREATE INDEX "withdrawals_agent_idx" ON "withdrawals" USING btree ("agent_id","created_at");