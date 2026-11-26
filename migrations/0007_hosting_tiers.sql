ALTER TABLE "agents" ADD COLUMN "tier" text DEFAULT 'basic' NOT NULL;--> statement-breakpoint
ALTER TABLE "agents" ADD COLUMN "hardware" jsonb;--> statement-breakpoint
ALTER TABLE "agents" ADD COLUMN "environment_hash" text;