CREATE TABLE "world_sessions" (
	"id" text PRIMARY KEY NOT NULL,
	"game_id" text NOT NULL,
	"account_id" text NOT NULL,
	"model" text NOT NULL,
	"gpu" boolean NOT NULL,
	"country" text,
	"preview" boolean DEFAULT false NOT NULL,
	"allowed_seconds" integer NOT NULL,
	"seconds" double precision,
	"ended_at" timestamp with time zone,
	"expires_at" timestamp with time zone NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE INDEX "world_sessions_account_idx" ON "world_sessions" USING btree ("account_id","created_at");--> statement-breakpoint
CREATE INDEX "world_sessions_game_idx" ON "world_sessions" USING btree ("game_id");