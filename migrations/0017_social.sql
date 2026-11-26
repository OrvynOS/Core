CREATE TABLE "social_profiles" (
	"subject" text PRIMARY KEY NOT NULL,
	"kind" text NOT NULL,
	"handle" text NOT NULL,
	"display_name" text NOT NULL,
	"bio" text DEFAULT '' NOT NULL,
	"avatar" text,
	"verified_at" timestamp with time zone NOT NULL,
	"suspended" boolean DEFAULT false NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"updated_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE UNIQUE INDEX "social_profiles_handle_idx" ON "social_profiles" USING btree ("handle");--> statement-breakpoint
CREATE TABLE "social_posts" (
	"id" bigserial PRIMARY KEY NOT NULL,
	"author" text NOT NULL,
	"text" text DEFAULT '' NOT NULL,
	"media" jsonb DEFAULT '[]'::jsonb NOT NULL,
	"reply_to" bigint,
	"repost_of" bigint,
	"reply_count" integer DEFAULT 0 NOT NULL,
	"repost_count" integer DEFAULT 0 NOT NULL,
	"via" text,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"deleted_at" timestamp with time zone
);
--> statement-breakpoint
CREATE INDEX "social_posts_author_idx" ON "social_posts" USING btree ("author","id");--> statement-breakpoint
CREATE INDEX "social_posts_reply_idx" ON "social_posts" USING btree ("reply_to","id");--> statement-breakpoint
CREATE INDEX "social_posts_top_idx" ON "social_posts" USING btree ("id") WHERE "reply_to" IS NULL AND "deleted_at" IS NULL;--> statement-breakpoint
CREATE UNIQUE INDEX "social_posts_repost_idx" ON "social_posts" USING btree ("author","repost_of") WHERE "repost_of" IS NOT NULL AND "deleted_at" IS NULL AND "text" = '';--> statement-breakpoint
CREATE TABLE "social_follows" (
	"follower" text NOT NULL,
	"followee" text NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "social_follows_pk" PRIMARY KEY("follower","followee")
);
--> statement-breakpoint
CREATE INDEX "social_follows_followee_idx" ON "social_follows" USING btree ("followee");--> statement-breakpoint
CREATE TABLE "social_media" (
	"id" text PRIMARY KEY NOT NULL,
	"owner" text NOT NULL,
	"mime" text NOT NULL,
	"bytes" bytea NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE INDEX "social_media_owner_idx" ON "social_media" USING btree ("owner","created_at");
