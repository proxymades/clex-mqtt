import { useEffect, useState } from 'react';
import type { ApiClient } from './api';
import type { Notify } from './App';
import { UsersPage } from './UsersPage';
import { GroupsPage } from './GroupsPage';
import { AccessCheckPage } from './AccessCheckPage';

export function AccessPage(props: { api: ApiClient; revision: number; notify: Notify; onChange: () => void }) {
  const [hash, setHash] = useState(() => window.location.hash);
  useEffect(() => {
    const changed = () => setHash(window.location.hash);
    window.addEventListener('hashchange', changed);
    return () => window.removeEventListener('hashchange', changed);
  }, []);
  const params = new URLSearchParams(hash.split('?')[1] ?? '');
  const tab = params.get('tab') ?? 'users';
  return <>
    <nav className="message-tabs access-tabs" aria-label="Вкладки доступа">
      <a href="#users" aria-current={tab === 'users' ? 'page' : undefined}>Учётные записи</a>
      <a href="#users?tab=groups" aria-current={tab === 'groups' ? 'page' : undefined}>Группы</a>
      <a href="#users?tab=check" aria-current={tab === 'check' ? 'page' : undefined}>Проверка доступа</a>
    </nav>
    {tab === 'groups' ? <GroupsPage {...props} /> : tab === 'check' ? <AccessCheckPage api={props.api} revision={props.revision} username={params.get('username') ?? ''} /> : <UsersPage {...props} groupId={params.get('groupId') ?? ''} />}
  </>;
}
