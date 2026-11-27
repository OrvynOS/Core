CREATE TABLE "inference_prefs" (
	"account_id" text PRIMARY KEY NOT NULL,
	"default_model" text NOT NULL,
	"updated_at" timestamp with time zone DEFAULT now() NOT NULL
);
