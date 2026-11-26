CREATE TABLE "vyn_channels" (
	"id" serial PRIMARY KEY NOT NULL,
	"account_id" text NOT NULL,
	"kind" text NOT NULL,
	"external_id" text NOT NULL,
	"thread_id" text NOT NULL,
	"label" text,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE TABLE "vyn_link_codes" (
	"code" text PRIMARY KEY NOT NULL,
	"account_id" text NOT NULL,
	"kind" text NOT NULL,
	"expires_at" timestamp with time zone NOT NULL
);
--> statement-breakpoint
CREATE TABLE "vyn_memories" (
	"id" serial PRIMARY KEY NOT NULL,
	"account_id" text NOT NULL,
	"text" text NOT NULL,
	"kind" text DEFAULT 'fact' NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE TABLE "vyn_notices" (
	"id" serial PRIMARY KEY NOT NULL,
	"account_id" text NOT NULL,
	"thread_id" text NOT NULL,
	"title" text NOT NULL,
	"text" text NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE TABLE "vyn_routines" (
	"id" text PRIMARY KEY NOT NULL,
	"account_id" text NOT NULL,
	"title" text NOT NULL,
	"instruction" text NOT NULL,
	"schedule" jsonb NOT NULL,
	"kind" text DEFAULT 'user' NOT NULL,
	"next_run_at" timestamp with time zone,
	"last_run_at" timestamp with time zone,
	"active" boolean DEFAULT true NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
ALTER TABLE "vyn_settings" ADD COLUMN "persona" jsonb DEFAULT '{}'::jsonb NOT NULL;--> statement-breakpoint
ALTER TABLE "vyn_settings" ADD COLUMN "timezone" text DEFAULT 'UTC' NOT NULL;--> statement-breakpoint
ALTER TABLE "vyn_settings" ADD COLUMN "check_in_time" text;--> statement-breakpoint
CREATE UNIQUE INDEX "vyn_channels_external_idx" ON "vyn_channels" USING btree ("kind","external_id");--> statement-breakpoint
CREATE INDEX "vyn_channels_account_idx" ON "vyn_channels" USING btree ("account_id");--> statement-breakpoint
CREATE INDEX "vyn_memories_account_idx" ON "vyn_memories" USING btree ("account_id","id");--> statement-breakpoint
CREATE INDEX "vyn_notices_account_idx" ON "vyn_notices" USING btree ("account_id","id");--> statement-breakpoint
CREATE INDEX "vyn_routines_due_idx" ON "vyn_routines" USING btree ("active","next_run_at");--> statement-breakpoint
CREATE INDEX "vyn_routines_account_idx" ON "vyn_routines" USING btree ("account_id");