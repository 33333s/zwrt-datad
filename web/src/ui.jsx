import React, {
  createContext,
  useContext,
  useEffect,
  useState,
  useId,
} from "react";
import {
  ActionIcon,
  Box,
  Button,
  Card,
  Group,
  SimpleGrid,
  Text,
  UnstyledButton,
} from "@mantine/core";
import { toast } from "../toasts.js";
import { AnimatePresence, motion } from "motion/react";
import { ChevronDown } from "lucide-react";
import { display } from "../core.js";

export const Services = createContext(null);
export const EfsStatus = createContext(null);
export const useServices = () => useContext(Services);
export const notify = (message, error = false) =>
  error ? toast.error(message) : toast.info(message);
export function Panel({
  title,
  description,
  icon: Icon,
  children,
  right,
  summary,
  collapsible = false,
  defaultOpen = true,
  className = "",
}) {
  const [open, setOpen] = useState(defaultOpen);
  const bodyId = useId();
  return (
    <Card withBorder radius="lg" padding="lg" className={"panel " + className}>
      <Group justify="space-between" wrap="nowrap" className="panel-heading">
        <Group gap={10} wrap="nowrap">
          {Icon && (
            <span className="section-icon">
              <Icon size={17} strokeWidth={1.8} />
            </span>
          )}
          <Box>
            <Text fw={600} size="sm">
              {title}
            </Text>
            {description && (
              <Text size="xs" c="dimmed" mt={3}>
                {description}
              </Text>
            )}
          </Box>
        </Group>
        <Group gap="xs" wrap="nowrap">
          {right}
          {collapsible && (
            <ActionIcon
              variant="subtle"
              color="gray"
              aria-label={open ? "收起" + title : "展开" + title}
              aria-expanded={open}
              aria-controls={bodyId}
              onClick={() => setOpen(!open)}
            >
              <motion.span animate={{ rotate: open ? 0 : -90 }}>
                <ChevronDown size={16} />
              </motion.span>
            </ActionIcon>
          )}
        </Group>
      </Group>
      {summary && <Box mt="sm">{summary}</Box>}
      <AnimatePresence initial={false}>
        {open && (
          <motion.div
            key="body"
            id={bodyId}
            initial={{ height: 0, opacity: 0 }}
            animate={{ height: "auto", opacity: 1 }}
            exit={{ height: 0, opacity: 0 }}
            transition={{ duration: 0.22, ease: [0.22, 1, 0.36, 1] }}
            className="panel-body"
          >
            <Box pt="lg">{children}</Box>
          </motion.div>
        )}
      </AnimatePresence>
    </Card>
  );
}
function PrivateValue({ label, value }) {
  const [revealed, setRevealed] = useState(false);
  const valueId = useId();
  return (
    <UnstyledButton
      className="private-value-button"
      aria-label={`${revealed ? "隐藏" : "显示"}${label}`}
      aria-pressed={revealed}
      aria-describedby={revealed ? valueId : undefined}
      title={revealed ? "点击隐藏" : "点击显示"}
      onClick={() => setRevealed((v) => !v)}
    >
      <Text
        component="span"
        size="sm"
        fw={500}
        className="value private-value"
        id={valueId}
        data-revealed={revealed || undefined}
        aria-hidden={!revealed}
      >
        {display(value)}
      </Text>
    </UnstyledButton>
  );
}
export function InfoGrid({ items, cols, sensitive = [] }) {
  return (
    <SimpleGrid
      cols={cols ?? { base: 2, md: 3, xl: 4 }}
      spacing="lg"
      verticalSpacing="md"
    >
      {items.map(([name, value]) => (
        <Box key={name} className="info-field">
          <Text size="xs" c="dimmed" mb={5}>
            {name}
          </Text>
          {sensitive.includes(name) &&
          value !== undefined &&
          value !== null &&
          value !== "" ? (
            <PrivateValue key={String(value)} label={name} value={value} />
          ) : (
            <Text size="sm" fw={500} className="value">
              {display(value)}
            </Text>
          )}
        </Box>
      ))}
    </SimpleGrid>
  );
}
export function CountdownButton({ seconds = 0, children, ...props }) {
  const [left, setLeft] = useState(seconds);
  useEffect(() => {
    const deadline = performance.now() + seconds * 1000;
    const tick = () =>
      setLeft(Math.max(0, Math.ceil((deadline - performance.now()) / 1000)));
    tick();
    const id = setInterval(tick, 150);
    return () => clearInterval(id);
  }, [seconds]);
  return (
    <Button {...props} disabled={props.disabled || left > 0}>
      {children}
      {left > 0 ? ` (${left}s)` : ""}
    </Button>
  );
}
