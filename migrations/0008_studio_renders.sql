CREATE TABLE "job_files" (
	"id" text PRIMARY KEY NOT NULL,
	"render_id" text NOT NULL,
	"agent_id" text NOT NULL,
	"hire_id" text,
	"name" text NOT NULL,
	"mime" text NOT NULL,
	"bytes" integer NOT NULL,
	"sha256" text NOT NULL,
	"worker_url" text NOT NULL,
	"check_ok" boolean NOT NULL,
	"check_notes" jsonb DEFAULT '[]'::jsonb NOT NULL,
	"delivered" boolean DEFAULT false NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE TABLE "studio_renders" (
	"id" text PRIMARY KEY NOT NULL,
	"agent_id" text NOT NULL,
	"hire_id" text,
	"renderer" text NOT NULL,
	"worker_url" text NOT NULL,
	"ok" boolean NOT NULL,
	"error" text,
	"duration_ms" integer NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE INDEX "job_files_hire_idx" ON "job_files" USING btree ("hire_id");--> statement-breakpoint
CREATE INDEX "studio_renders_hire_idx" ON "studio_renders" USING btree ("hire_id");--> statement-breakpoint
CREATE INDEX "studio_renders_agent_idx" ON "studio_renders" USING btree ("agent_id","created_at");