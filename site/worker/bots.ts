/**
 * Whether a presence connection (worker/presence.ts) comes from a crawler or other automation, decided here in the
 * worker at the WebSocket upgrade, never by the client. Crawlers are welcome on the site; this only changes how they
 * show to the readers: as a robot named for the crawler ("Googlebot"), in the who-is-here list's "Crawlers" group, not
 * counted online, with no join or leave lines, no pointer and no chat.
 *
 * The signals, strongest first:
 *
 * - `request.cf.verifiedBotCategory`: Cloudflare's category of a verified bot (one it checked by reverse DNS, its
 *   published IP ranges or Web Bot Auth: "Search Engine Crawler", "AI Crawler", "Page Preview"); empty otherwise.
 * - `request.cf.botManagement`: `verifiedBot`, and `score` 1 to 29 (Cloudflare's "likely automated"). Real only with
 *   the Enterprise Bot Management add-on; on any other plan (ix.dev is on Free) the object is there with fixed dummy
 *   values (`score: 99`, `verifiedBot: false`), which this never takes as a bot.
 * - The user agent: well-known crawlers by name, then anything that says it is a bot, crawler, spider, a headless
 *   browser (HeadlessChrome, which Playwright's and Puppeteer's headless shells send) or an HTTP library. A spoofed user
 *   agent only makes someone look like a bot, which costs them a pointer and a seat in the chat, nothing else.
 *
 * A signed-in reader is never a bot (worker/presence.ts doesn't ask).
 */

/** Crawlers by the token in their user agent, and the name readers see. First match wins, so the specific go first. */
const NAMED: [RegExp, string][] = [
	[/Google-InspectionTool/i, 'Google Inspection Tool'],
	[/AdsBot-Google/i, 'Google AdsBot'],
	[/Mediapartners-Google/i, 'Google AdSense'],
	[/Storebot-Google/i, 'Google StoreBot'],
	[/GoogleOther/i, 'GoogleOther'],
	[/Google-Extended/i, 'Google-Extended'],
	[/Googlebot/i, 'Googlebot'],
	[/Chrome-Lighthouse|Lighthouse/i, 'Lighthouse'],
	[/bingbot|BingPreview|msnbot/i, 'Bingbot'],
	[/OAI-SearchBot/i, 'OAI-SearchBot'],
	[/ChatGPT-User/i, 'ChatGPT-User'],
	[/GPTBot/i, 'GPTBot'],
	[/Claude-User/i, 'Claude-User'],
	[/Claude-SearchBot/i, 'Claude-SearchBot'],
	[/ClaudeBot|anthropic-ai/i, 'ClaudeBot'],
	[/Perplexity-User/i, 'Perplexity-User'],
	[/PerplexityBot/i, 'PerplexityBot'],
	[/Applebot-Extended/i, 'Applebot-Extended'],
	[/Applebot/i, 'Applebot'],
	[/DuckAssistBot/i, 'DuckAssistBot'],
	[/DuckDuckBot|DuckDuckGo-Favicons-Bot/i, 'DuckDuckBot'],
	[/YandexBot|YandexImages|YandexRenderResourcesBot|Yandex(?:\w*)Bot/i, 'YandexBot'],
	[/Baiduspider/i, 'Baiduspider'],
	[/Yahoo! Slurp|\bSlurp\b/i, 'Yahoo Slurp'],
	[/Amazonbot/i, 'Amazonbot'],
	[/Bytespider/i, 'Bytespider'],
	[/CCBot/i, 'CCBot'],
	[/meta-externalagent|meta-externalfetcher/i, 'Meta crawler'],
	[/facebookexternalhit|facebookcatalog/i, 'Facebook preview'],
	[/Twitterbot/i, 'Twitterbot'],
	[/LinkedInBot/i, 'LinkedInBot'],
	[/Slackbot/i, 'Slackbot'],
	[/Discordbot/i, 'Discordbot'],
	[/TelegramBot/i, 'TelegramBot'],
	[/WhatsApp/i, 'WhatsApp preview'],
	[/redditbot/i, 'Redditbot'],
	[/Mastodon/i, 'Mastodon preview'],
	[/AhrefsBot|AhrefsSiteAudit/i, 'AhrefsBot'],
	[/SemrushBot|SiteAuditBot/i, 'SemrushBot'],
	[/MJ12bot/i, 'MJ12bot'],
	[/DotBot/i, 'DotBot'],
	[/PetalBot/i, 'PetalBot'],
	[/SeznamBot/i, 'SeznamBot'],
	[/Qwantbot|Qwantify/i, 'Qwantbot'],
	[/Sogou/i, 'Sogou spider'],
	[/ia_archiver|archive\.org_bot/i, 'Internet Archive'],
	[/MojeekBot/i, 'MojeekBot'],
	[/Kagibot/i, 'Kagibot'],
	[/YouBot/i, 'YouBot'],
	[/cohere-ai/i, 'Cohere crawler'],
	[/Diffbot/i, 'Diffbot'],
	[/HeadlessChrome|PhantomJS|Puppeteer|Playwright|Selenium|webdriver/i, 'A headless browser'],
];

/**
 * Anything else that says it is automation. `bot` as a word or a product token (`Somebot/1.0`, `some-bot`), but not
 * inside a word, so a CUBOT phone or "Abbott" is a person.
 */
const GENERIC =
	/(?<![a-z])bot(?![a-z])|[a-z]bot(?:[\/_;\s)-]|$)|crawl|spider|scrap(?:er|y)|fetcher|validator|monitor(?:ing)?\b|preview|\bcurl\/|\bwget\/|python-(?:requests|urllib|httpx)|aiohttp|go-http-client|okhttp|axios\/|node-fetch|undici|java\/\d|libwww|httpclient|\bhttpie\//i;
/** Words that end in "bot" and are not one. */
const NOT_BOT = /cubot|abbott/i;

/** Cloudflare's verified-bot categories (`request.cf.verifiedBotCategory`) as a name, when the user agent named none. */
const CATEGORY: Record<string, string> = {
	'Search Engine Crawler': 'A search crawler',
	'AI Crawler': 'An AI crawler',
	'AI Search': 'An AI search crawler',
	'AI Assistant': 'An AI assistant',
	'Page Preview': 'A link preview',
	Archiver: 'An archiver',
	'Monitoring & Analytics': 'A monitor',
};

/** The name a bot shows as, or null for a person. */
export function botName(ua: string, raw: Request['cf']): string | null {
	// workers-types doesn't declare `verifiedBotCategory`, and `botManagement` may be missing: read both as unknowns.
	const cf = (raw ?? {}) as { verifiedBotCategory?: unknown; botManagement?: { verifiedBot?: unknown; score?: unknown } | null };
	const category = typeof cf.verifiedBotCategory === 'string' ? cf.verifiedBotCategory : '';
	const bm = cf.botManagement ?? undefined;
	const verified = category !== '' || bm?.verifiedBot === true;
	// 1 to 29 is "likely automated". 0 is "not computed", and the dummy object of a plan without Bot Management says 99.
	const likely = typeof bm?.score === 'number' && bm.score >= 1 && bm.score < 30;
	for (const [re, name] of NAMED) if (re.test(ua)) return name;
	if (verified) return CATEGORY[category] ?? 'A crawler';
	if (GENERIC.test(ua) && !NOT_BOT.test(ua)) return 'A crawler';
	if (!ua.trim()) return 'A bot';
	return likely ? 'A bot' : null;
}
