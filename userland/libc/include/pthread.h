#ifndef _PTHREAD_H
#define _PTHREAD_H

#include <stddef.h>
#include <time.h>

typedef unsigned long pthread_t;

typedef struct {
    unsigned int __state;
} pthread_mutex_t;

typedef struct {
    unsigned int __seq;
} pthread_cond_t;

typedef struct {
    pthread_mutex_t __mutex;
} pthread_rwlock_t;

typedef struct {
    unsigned int __state;
} pthread_once_t;

typedef unsigned long pthread_mutexattr_t;
typedef unsigned long pthread_condattr_t;
typedef unsigned long pthread_rwlockattr_t;
typedef unsigned long pthread_attr_t;
typedef unsigned int pthread_key_t;

#define PTHREAD_MUTEX_INITIALIZER  { 0 }
#define PTHREAD_COND_INITIALIZER   { 0 }
#define PTHREAD_RWLOCK_INITIALIZER { { 0 } }
#define PTHREAD_ONCE_INIT          { 0 }

#define PTHREAD_MUTEX_NORMAL     0
#define PTHREAD_MUTEX_RECURSIVE  1
#define PTHREAD_MUTEX_ERRORCHECK 2
#define PTHREAD_MUTEX_DEFAULT    PTHREAD_MUTEX_NORMAL

#define PTHREAD_CREATE_JOINABLE 0
#define PTHREAD_CREATE_DETACHED 1

/* Thread */
int pthread_create(pthread_t *thread, const pthread_attr_t *attr,
                   void *(*start_routine)(void *), void *arg);
int pthread_join(pthread_t thread, void **retval);
int pthread_detach(pthread_t thread);
pthread_t pthread_self(void);
int pthread_equal(pthread_t t1, pthread_t t2);

/* Mutex */
int pthread_mutex_init(pthread_mutex_t *mutex, const pthread_mutexattr_t *attr);
int pthread_mutex_lock(pthread_mutex_t *mutex);
int pthread_mutex_trylock(pthread_mutex_t *mutex);
int pthread_mutex_unlock(pthread_mutex_t *mutex);
int pthread_mutex_destroy(pthread_mutex_t *mutex);

int pthread_mutexattr_init(pthread_mutexattr_t *attr);
int pthread_mutexattr_destroy(pthread_mutexattr_t *attr);
int pthread_mutexattr_settype(pthread_mutexattr_t *attr, int type);

/* Condition variable */
int pthread_cond_init(pthread_cond_t *cond, const pthread_condattr_t *attr);
int pthread_cond_wait(pthread_cond_t *cond, pthread_mutex_t *mutex);
int pthread_cond_signal(pthread_cond_t *cond);
int pthread_cond_broadcast(pthread_cond_t *cond);
int pthread_cond_destroy(pthread_cond_t *cond);

int pthread_condattr_init(pthread_condattr_t *attr);
int pthread_condattr_destroy(pthread_condattr_t *attr);

/* Read-write lock */
int pthread_rwlock_init(pthread_rwlock_t *rwlock, const pthread_rwlockattr_t *attr);
int pthread_rwlock_rdlock(pthread_rwlock_t *rwlock);
int pthread_rwlock_wrlock(pthread_rwlock_t *rwlock);
int pthread_rwlock_unlock(pthread_rwlock_t *rwlock);
int pthread_rwlock_destroy(pthread_rwlock_t *rwlock);

/* Once */
int pthread_once(pthread_once_t *once_control, void (*init_routine)(void));

/* Thread-local storage */
int pthread_key_create(pthread_key_t *key, void (*destructor)(void *));
int pthread_key_delete(pthread_key_t key);
void *pthread_getspecific(pthread_key_t key);
int pthread_setspecific(pthread_key_t key, const void *value);

/* Attributes */
int pthread_attr_init(pthread_attr_t *attr);
int pthread_attr_destroy(pthread_attr_t *attr);
int pthread_attr_setstacksize(pthread_attr_t *attr, size_t stacksize);
int pthread_attr_getstacksize(const pthread_attr_t *attr, size_t *stacksize);
int pthread_attr_setdetachstate(pthread_attr_t *attr, int detachstate);

#endif
