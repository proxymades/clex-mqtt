import { Copy } from 'lucide-react';

interface Props {
  values: string[];
  kind: string;
  label: string;
  copy: (value: string) => Promise<void>;
}

export function MqttTopicFilters({ values, kind, label, copy }: Props) {
  return values.length ? <div className="mqtt-topic-filters">{values.map((value) =>
    <div className="mqtt-topic-chip" key={value}>
      <code>{value}</code>
      <button type="button" className="icon-button" title="Скопировать фильтр" aria-label={'Скопировать фильтр ' + kind + ' ' + value + ' для ' + label} onClick={() => void copy(value)}><Copy size={14} /></button>
    </div>
  )}</div> : <span className="muted">Нет разрешений</span>;
}
