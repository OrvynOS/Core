CREATE TABLE "game_assets" (
	"game_id" text NOT NULL,
	"name" text NOT NULL,
	"mime" text NOT NULL,
	"bytes" "bytea" NOT NULL,
	"size" integer NOT NULL,
	"sha256" text NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "game_assets_game_id_name_pk" PRIMARY KEY("game_id","name")
);
--> statement-breakpoint
CREATE TABLE "game_payments" (
	"tx_hash" text PRIMARY KEY NOT NULL,
	"game_id" text NOT NULL,
	"account_id" text NOT NULL,
	"kind" text NOT NULL,
	"token" text NOT NULL,
	"amount" text NOT NULL,
	"usd" double precision,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE TABLE "game_versions" (
	"game_id" text NOT NULL,
	"version" integer NOT NULL,
	"game" jsonb NOT NULL,
	"sha256" text NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "game_versions_game_id_version_pk" PRIMARY KEY("game_id","version")
);
--> statement-breakpoint
CREATE TABLE "games" (
	"id" text PRIMARY KEY NOT NULL,
	"slug" text NOT NULL,
	"title" text NOT NULL,
	"description" text DEFAULT '' NOT NULL,
	"creator_account_id" text NOT NULL,
	"agent_id" text,
	"publish_as_agent" boolean DEFAULT false NOT NULL,
	"hire_id" text,
	"status" text DEFAULT 'draft' NOT NULL,
	"access" jsonb DEFAULT '{"kind":"free","tips":true}'::jsonb NOT NULL,
	"draft" jsonb NOT NULL,
	"problems" jsonb DEFAULT '[]'::jsonb NOT NULL,
	"version" integer DEFAULT 0 NOT NULL,
	"plays" integer DEFAULT 0 NOT NULL,
	"published_at" timestamp with time zone,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"updated_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "games_slug_unique" UNIQUE("slug")
);
--> statement-breakpoint
CREATE INDEX "game_payments_game_idx" ON "game_payments" USING btree ("game_id","account_id");--> statement-breakpoint
CREATE INDEX "games_creator_idx" ON "games" USING btree ("creator_account_id");--> statement-breakpoint
CREATE INDEX "games_agent_idx" ON "games" USING btree ("agent_id");--> statement-breakpoint
CREATE INDEX "games_status_idx" ON "games" USING btree ("status","published_at");