import ReactMarkdown from "react-markdown";
import remarkBreaks from "remark-breaks";

// Renders chat prose (user and assistant messages, composer preview) as
// real markdown instead of raw text. remark-breaks keeps a single "\n"
// as a visual line break -- models don't reliably blank-line-separate
// paragraphs the way strict CommonMark expects. No raw-HTML plugin is
// enabled, so markdown source containing HTML renders as escaped text
// rather than executing -- safe by default against model/tool output.
export function Markdown({ children, className }: { children: string; className?: string }) {
  return (
    <div className={className ? `md ${className}` : "md"}>
      <ReactMarkdown remarkPlugins={[remarkBreaks]}>{children}</ReactMarkdown>
    </div>
  );
}
