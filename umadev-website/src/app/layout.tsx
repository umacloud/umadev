import type { Metadata } from "next";
import { JetBrains_Mono, Manrope, Space_Grotesk } from "next/font/google";
import "./globals.css";
import { asset } from "./content";

const manrope = Manrope({
  variable: "--font-manrope",
  subsets: ["latin"],
  display: "swap",
});

const spaceGrotesk = Space_Grotesk({
  variable: "--font-space",
  subsets: ["latin"],
  display: "swap",
});

const jetBrainsMono = JetBrains_Mono({
  variable: "--font-mono",
  subsets: ["latin"],
  display: "swap",
});

export const metadata: Metadata = {
  metadataBase: new URL(process.env.NEXT_PUBLIC_SITE_URL ?? "https://umadev.goder.ai"),
  title: "UmaDev — 一个模拟真实开发团队、驱动你的底座干活的 Agent",
  description:
    "UmaDev 驱动你已登录的 Claude Code、Codex 或 OpenCode，由八个专家角色完成任务路由、可视计划、独立评审、真实验证与交付证明。",
  icons: {
    icon: asset("/assets/umadev-icon.png"),
    apple: asset("/assets/umadev-icon.png"),
  },
  openGraph: {
    title: "UmaDev — One agent. A whole development team at work.",
    description:
      "Drive your logged-in Claude Code, Codex, or OpenCode with task routing, a visible plan, eight specialist roles, independent review, persistent context, deterministic verification, and delivery evidence.",
    type: "website",
    images: [{ url: asset("/assets/wide-1.png"), width: 1672, height: 941 }],
  },
};

export default function RootLayout({
  children,
}: Readonly<{
  children: React.ReactNode;
}>) {
  return (
    <html
      lang="zh-CN"
      className={`${manrope.variable} ${spaceGrotesk.variable} ${jetBrainsMono.variable}`}
      suppressHydrationWarning
    >
      <body suppressHydrationWarning>{children}</body>
    </html>
  );
}
